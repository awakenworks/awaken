//! Tests for the SQLite commit boundary. SQLite is embedded, so these always
//! run (no external server, no skip): an in-memory database covers commit, the
//! fence, reads, and waiting tickets; a temp file covers rehydration on restart.

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::state::{Command as StateCommand, MergePolicy, Scope};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::agent::waiting::{WaitingReason, WaitingTicket};
use awaken_agent_contract::commit::coordinator::Coordinator;
use awaken_agent_contract::commit::staged::ThreadCommit;
use awaken_agent_contract::event::draft::Draft;
use awaken_agent_contract::event::kind::Kind as EventKind;
use awaken_agent_contract::fact::run::Fact as RunFact;
use awaken_agent_contract::store::run_store::RunStore;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_store_sqlite::SqliteCommitCoordinator;

fn message(id: &str, text: &str) -> Message {
    Message {
        id: MessageId(id.to_string()),
        role: Role::Assistant,
        content: vec![ContentBlock::text(text)],
    }
}

fn ended(run: &str) -> RunFact {
    RunFact {
        run_id: RunId(run.to_string()),
        phase: Phase::Ended(EndCause::NaturalEnd),
    }
}

fn waiting_fact(run: &str) -> RunFact {
    RunFact {
        run_id: RunId(run.to_string()),
        phase: Phase::Waiting,
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

fn empty_commit(thread: &str, fact: RunFact, waiting: Option<WaitingTicket>) -> ThreadCommit {
    ThreadCommit {
        thread_id: ThreadId(thread.to_string()),
        run_fact: fact,
        messages: vec![],
        state: vec![],
        events: vec![],
        waiting,
    }
}

#[tokio::test]
async fn commit_persists_facts_messages_and_serves_reads() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    let thread = ThreadId("thread-1".to_string());

    let commit = ThreadCommit {
        thread_id: thread.clone(),
        run_fact: ended("run-1"),
        messages: vec![message("m1", "hello"), message("m2", "world")],
        state: vec![StateCommand::set(
            Scope::Run,
            MergePolicy::Disjoint,
            "k",
            serde_json::json!("v"),
        )],
        events: vec![Draft {
            kind: EventKind::MessageCommitted,
            payload: serde_json::json!({"n": 1}),
        }],
        waiting: None,
    };

    let record = store.commit(commit).await.expect("commit");
    assert_eq!(record.sequence, 1);
    assert_eq!(store.commit_count(), 1);

    let run = RunStore::get(&store, &RunId("run-1".to_string())).expect("run record");
    assert_eq!(run.phase, Phase::Ended(EndCause::NaturalEnd));
    assert_eq!(run.thread_id, thread);

    let messages = ThreadReader::committed_messages(&store, &thread);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].id.0, "m1");
    assert_eq!(messages[1].text_content(), "world");
}

#[tokio::test]
async fn fence_increments_monotonically() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    for expected in 1..=3u64 {
        let record = store
            .commit(empty_commit("thread-1", ended("run-1"), None))
            .await
            .expect("commit");
        assert_eq!(record.sequence, expected);
    }
    assert_eq!(store.commit_count(), 3);
}

#[tokio::test]
async fn waiting_ticket_parks_then_clears() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    let run = RunId("run-1".to_string());

    store
        .commit(empty_commit(
            "thread-1",
            waiting_fact("run-1"),
            Some(ticket("run-1", "thread-1")),
        ))
        .await
        .expect("park");
    assert!(ThreadReader::waiting_ticket(&store, &run).is_some());

    store
        .commit(empty_commit("thread-1", ended("run-1"), None))
        .await
        .expect("resume to terminal");
    assert!(
        ThreadReader::waiting_ticket(&store, &run).is_none(),
        "a terminal run clears its ticket (fail closed)"
    );
}

#[tokio::test]
async fn projection_rehydrates_from_a_file_after_reopen() {
    let path = std::env::temp_dir().join(format!(
        "awaken_store_sqlite_rehydrate_{}.db",
        std::process::id()
    ));
    let path = path.to_str().unwrap().to_string();
    let _ = std::fs::remove_file(&path);

    {
        let store = SqliteCommitCoordinator::open(&path).expect("open a");
        store
            .commit(ThreadCommit {
                thread_id: ThreadId("thread-1".to_string()),
                run_fact: waiting_fact("run-1"),
                messages: vec![message("m1", "persisted")],
                state: vec![],
                events: vec![],
                waiting: Some(ticket("run-1", "thread-1")),
            })
            .await
            .expect("commit");
    } // store dropped — simulate a restart

    let restarted = SqliteCommitCoordinator::open(&path).expect("open b");
    assert_eq!(restarted.commit_count(), 1, "the fence survives restart");
    assert_eq!(
        ThreadReader::committed_messages(&restarted, &ThreadId("thread-1".to_string()))[0]
            .id
            .0,
        "m1"
    );
    assert!(RunStore::get(&restarted, &RunId("run-1".to_string())).is_some());
    assert!(
        ThreadReader::waiting_ticket(&restarted, &RunId("run-1".to_string())).is_some(),
        "the active ticket rehydrated"
    );

    let _ = std::fs::remove_file(&path);
}

// G1 guardrail: SqliteCommitCoordinator must reject malformed commits before
// writing. The fence must not advance when validate() fires.

#[tokio::test]
async fn sqlite_rejects_empty_thread_id_before_write() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    let bad = ThreadCommit {
        thread_id: ThreadId("".to_string()),
        run_fact: ended("run-1"),
        messages: vec![],
        state: vec![],
        events: vec![],
        waiting: None,
    };
    let err = store.commit(bad).await.expect_err("must reject");
    assert!(
        err.to_string().contains("thread_id"),
        "error names the offending field"
    );
    assert_eq!(
        store.commit_count(),
        0,
        "fence must not advance on rejection"
    );
}

#[tokio::test]
async fn sqlite_rejects_empty_run_id_before_write() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    let bad = ThreadCommit {
        thread_id: ThreadId("thread-1".to_string()),
        run_fact: RunFact {
            run_id: RunId("".to_string()),
            phase: Phase::Ended(EndCause::NaturalEnd),
        },
        messages: vec![],
        state: vec![],
        events: vec![],
        waiting: None,
    };
    let err = store.commit(bad).await.expect_err("must reject");
    assert!(
        err.to_string().contains("run_id"),
        "error names the offending field"
    );
    assert_eq!(
        store.commit_count(),
        0,
        "fence must not advance on rejection"
    );
}

#[tokio::test]
async fn sqlite_rejects_mismatched_waiting_ticket_before_write() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    let mismatched_ticket = WaitingTicket {
        correlation_id: "corr-x".to_string(),
        run_id: RunId("other-run".to_string()), // wrong run_id
        thread_id: ThreadId("thread-1".to_string()),
        snapshot_id: "snap-1".to_string(),
        catalog_fingerprint: "fp-1".to_string(),
        reason: WaitingReason::ToolPermission,
        call_id: None,
        pending_tool: None,
        deadline_ms: None,
    };
    let bad = ThreadCommit {
        thread_id: ThreadId("thread-1".to_string()),
        run_fact: waiting_fact("run-1"),
        messages: vec![],
        state: vec![],
        events: vec![],
        waiting: Some(mismatched_ticket),
    };
    let err = store.commit(bad).await.expect_err("must reject");
    assert!(
        err.to_string().contains("run_id"),
        "error describes the mismatch"
    );
    assert_eq!(
        store.commit_count(),
        0,
        "fence must not advance on rejection"
    );
}

#[tokio::test]
async fn sqlite_valid_commit_succeeds_after_prior_rejection() {
    // A rejected commit must leave the store fully intact so a subsequent valid
    // commit can proceed normally (no poisoned state).
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");

    let bad = ThreadCommit {
        thread_id: ThreadId("".to_string()),
        run_fact: ended("run-1"),
        messages: vec![],
        state: vec![],
        events: vec![],
        waiting: None,
    };
    store.commit(bad).await.expect_err("bad commit rejected");
    assert_eq!(store.commit_count(), 0);

    let good = empty_commit("thread-1", ended("run-1"), None);
    let record = store.commit(good).await.expect("valid commit succeeds");
    assert_eq!(
        record.sequence, 1,
        "fence advanced exactly once for the valid commit"
    );
}
