//! Tests for the SQLite commit boundary. SQLite is embedded, so these always
//! run (no external server, no skip): an in-memory database covers commit, the
//! fence, reads, and waiting tickets; a temp file covers rehydration on restart.

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::state::{Command as StateCommand, MergePolicy, Scope};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::agent::waiting::{WaitingReason, WaitingTicket};
use awaken_agent_contract::audit::draft::Draft;
use awaken_agent_contract::audit::kind::Kind as EventKind;
use awaken_agent_contract::commit::coordinator::Coordinator;
use awaken_agent_contract::commit::staged::ThreadCommit;
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
            kind: EventKind::RunPhaseChanged,
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
    // A distinct run per commit: the fence increments across successive commits,
    // which is what this pins. (Re-committing one terminal run would trip the
    // terminal-is-final guard, which fences a stale owner's duplicate post-terminal
    // commit; a run ends exactly once.)
    for expected in 1..=3u64 {
        let record = store
            .commit(empty_commit(
                "thread-1",
                ended(&format!("run-{expected}")),
                None,
            ))
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

// G13: before any commit the projection has no truth — no partial state is
// pre-visible from a staged but uncommitted plan.
#[tokio::test]
async fn g13_projection_absent_before_commit() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    assert!(
        RunStore::get(&store, &RunId("run-1".to_string())).is_none(),
        "projection must be empty before any commit"
    );
    assert_eq!(store.commit_count(), 0);
}

// G13: a rejected commit (invalid plan) writes nothing to the durable store —
// no partial state is observable after a failed `commit()` call.
#[tokio::test]
async fn g13_failed_commit_leaves_no_partial_state() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");

    // Empty thread_id fails validate() before the SQLite transaction opens.
    let err = store
        .commit(ThreadCommit {
            thread_id: ThreadId(String::new()),
            run_fact: ended("run-1"),
            messages: vec![],
            state: vec![],
            events: vec![],
            waiting: None,
        })
        .await;
    assert!(err.is_err(), "invalid plan must be rejected");

    assert!(
        RunStore::get(&store, &RunId("run-1".to_string())).is_none(),
        "no partial state after failed commit"
    );
    assert_eq!(
        store.commit_count(),
        0,
        "fence unchanged after rejected commit"
    );
}
