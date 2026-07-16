//! SQLite passes the shared store conformance suite, and a fresh instance over
//! the same database file resumes from committed facts (ADR-0039 2.5 / D4).

use awaken_agent_contract::agent::message::{Id as MsgId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::draft::Draft;
use awaken_agent_contract::audit::kind::Kind as EventKind;
use awaken_agent_contract::commit::RunFact;
use awaken_agent_contract::commit::coordinator::Coordinator;
use awaken_agent_contract::commit::staged::ThreadCommit;
use awaken_agent_contract::store::checkpoint::{CheckpointReader, EventScope};
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
                run_fact: RunFact {
                    run_id: run.clone(),
                    phase: Phase::Ended(EndCause::NaturalEnd),
                },
                messages: vec![Message::text(
                    MsgId("m1".to_string()),
                    Role::Assistant,
                    "hi",
                )],
                state: Vec::new(),
                events: vec![Draft {
                    kind: EventKind::RunPhaseChanged,
                    payload: serde_json::Value::Null,
                }],
                waiting: None,
            })
            .await
            .expect("commit");
        // store dropped — the in-memory projection is gone; only the DB file remains
    }

    let store = SqliteCommitCoordinator::open(path).expect("reopen");
    assert_eq!(store.committed_messages(&thread).len(), 1);
    assert_eq!(
        store.run(&run).map(|record| record.phase),
        Some(Phase::Ended(EndCause::NaturalEnd)),
    );
    assert_eq!(
        store.latest_run(&thread).map(|record| record.id),
        Some(run.clone())
    );
    assert_eq!(store.list_events(&EventScope::Run(run), None, 10).len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}
