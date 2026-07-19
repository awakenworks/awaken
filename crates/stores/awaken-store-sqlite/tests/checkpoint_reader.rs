//! SQLite passes the shared store conformance suite, and a fresh instance over
//! the same database file resumes from committed facts (ADR-0039 2.5 / D4).

use awaken_agent_contract::agent::message::{Id as MsgId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::draft::Draft;
use awaken_agent_contract::audit::kind::Kind as EventKind;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::Coordinator;
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::checkpoint::{CheckpointReader, EventScope};
use awaken_store_sqlite::SqliteCommitCoordinator;

#[tokio::test]
async fn conformance_commit_then_read() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    awaken_store_conformance::commit_then_read(&store).await;
}

#[tokio::test]
async fn conformance_events_ordered_and_paged() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    awaken_store_conformance::events_ordered_and_paged(&store).await;
}

#[tokio::test]
async fn conformance_commits_accumulate() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    awaken_store_conformance::commits_accumulate(&store).await;
}

#[tokio::test]
async fn conformance_terminal_run_is_fenced() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    awaken_store_conformance::terminal_run_is_fenced(&store).await;
}

#[tokio::test]
async fn conformance_resume_ticket_awaits_then_clears() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    awaken_store_conformance::resume_ticket_awaits_then_clears(&store).await;
}

#[tokio::test]
async fn conformance_concurrent_appends_are_dense_and_distinct() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    awaken_store_conformance::concurrent_appends_are_dense_and_distinct(&store).await;
}

// SQLite keys messages/runs by thread, so it isolates two threads in one store —
// it runs the shared multi-thread isolation case (the in-memory/fs reference
// backends cannot, and skip it).
#[tokio::test]
async fn conformance_two_threads_in_one_store_are_isolated() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    awaken_store_conformance::two_threads_in_one_store_are_isolated(&store).await;
}

#[tokio::test]
async fn conformance_empty_store_reads_are_absent() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    awaken_store_conformance::empty_store_reads_are_absent(&store).await;
}

// SQLite projects committed state commands back through the `committed_state` read
// port (rebuilt from the durable `runtime_state_command` rows), so a resumed run
// replays its accumulated state from durable truth — it runs the shared
// `committed_state_replays` conformance case.
#[tokio::test]
async fn conformance_committed_state_replays() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    awaken_store_conformance::committed_state_replays(&store).await;
}

#[tokio::test]
async fn conformance_delegation_and_tool_state_commit_atomically() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    awaken_store_conformance::delegation_and_tool_state_commit_atomically(&store).await;
}

#[tokio::test]
async fn reopen_file_resumes_from_committed_facts() {
    let dir = std::env::temp_dir().join("awaken_store_sqlite_reopen");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("commit.db");
    let path = path.to_str().expect("utf8 path");

    let thread = ThreadId("t1".to_string());
    let run = RunId("r1".to_string());

    {
        let store = SqliteCommitCoordinator::open(path).expect("open");
        store
            .commit(ThreadCommit {
                thread_id: thread.clone(),
                run: RunDisposition::ended(run.clone(), EndCause::NaturalEnd),
                messages: vec![Message::text(
                    MsgId("m1".to_string()),
                    Role::Assistant,
                    "hi",
                )],
                state: Vec::new(),
                events: vec![Draft {
                    kind: EventKind::RunStateChanged,
                    payload: serde_json::Value::Null,
                }],
            })
            .await
            .expect("commit");
        // store dropped — the in-memory projection is gone; only the DB file remains
    }

    let store = SqliteCommitCoordinator::open(path).expect("reopen");
    assert_eq!(store.committed_messages(&thread).len(), 1);
    assert_eq!(
        store.run(&run).map(|record| record.state),
        Some(RunState::Ended(EndCause::NaturalEnd)),
    );
    assert_eq!(
        store.latest_run(&thread).map(|record| record.id),
        Some(run.clone())
    );
    assert_eq!(store.list_events(&EventScope::Run(run), None, 10).len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

// Durable state replay across a reopen: committed state commands are served back
// through `committed_state` from a fresh instance over the same DB file, proving
// the fix survives a process restart (not just an in-process projection advance).
#[tokio::test]
async fn reopen_file_replays_committed_state() {
    use awaken_agent_contract::agent::state::{Command as StateCommand, MergePolicy, Scope};
    use awaken_agent_contract::thread::read::thread_reader::ThreadReader;

    let dir = std::env::temp_dir().join("awaken_store_sqlite_reopen_state");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("commit.db");
    let path = path.to_str().expect("utf8 path");

    let thread = ThreadId("t-state".to_string());
    let commands = vec![
        StateCommand::set(
            Scope::Thread,
            MergePolicy::Disjoint,
            "k1",
            serde_json::json!("v1"),
        ),
        StateCommand::set(
            Scope::Run,
            MergePolicy::Commutative,
            "k2",
            serde_json::json!(2),
        ),
    ];

    {
        let store = SqliteCommitCoordinator::open(path).expect("open");
        store
            .commit(ThreadCommit {
                thread_id: thread.clone(),
                run: RunDisposition::running(RunId("r-state".to_string())),
                messages: Vec::new(),
                state: commands.clone(),
                events: Vec::new(),
            })
            .await
            .expect("commit state");
        // store dropped — the in-memory projection is gone; only the DB file remains
    }

    let store = SqliteCommitCoordinator::open(path).expect("reopen");
    assert_eq!(
        ThreadReader::committed_state(&store, &thread),
        commands,
        "committed state replays from durable truth after a reopen"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
