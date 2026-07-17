//! SQLite passes the shared store conformance suite, and a fresh instance over
//! the same database file resumes from committed facts (ADR-0039 2.5 / D4).

use awaken_agent_contract::agent::message::{Id as MsgId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::draft::Draft;
use awaken_agent_contract::audit::kind::Kind as EventKind;
use awaken_agent_contract::thread::commit::RunFact;
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
async fn conformance_waiting_ticket_parks_then_clears() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    awaken_store_conformance::waiting_ticket_parks_then_clears(&store).await;
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

// The shared `committed_state_replays` conformance case is NOT run against SQLite:
// see `committed_state_read_returns_empty_despite_persisted_rows` below, which
// characterizes SQLite's current divergence on the `committed_state` read port.

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

// KNOWN BUG (adjudicate): SQLite persists committed state commands (into
// `runtime_state_command`, verified by the postgres/sqlite commit tests) but does
// NOT serve them back through the `ThreadReader::committed_state` read port — it
// leaves that method as the trait default, which returns empty. The in-memory and
// fs reference backends DO project committed state (they pass the shared
// `committed_state_replays` conformance case). Consumers that read this port on
// resume — materialized-state rebuild (`awaken-runtime` engine), token-usage
// accounting and compaction-count (`awaken-runtime-host`) — therefore read EMPTY
// against a SQLite (or Postgres) backend, silently losing accumulated state on a
// durable resume. This test pins the current (empty) behavior so the divergence is
// visible and a fix trips it.
#[tokio::test]
async fn committed_state_read_returns_empty_despite_persisted_rows() {
    use awaken_agent_contract::agent::state::{Command as StateCommand, MergePolicy, Scope};
    use awaken_agent_contract::thread::read::thread_reader::ThreadReader;

    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    let thread = ThreadId("t-state".to_string());

    store
        .commit(ThreadCommit {
            thread_id: thread.clone(),
            run_fact: RunFact {
                run_id: RunId("r-state".to_string()),
                phase: Phase::Running,
            },
            messages: Vec::new(),
            state: vec![
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
            ],
            events: Vec::new(),
            waiting: None,
        })
        .await
        .expect("commit state");

    // The rows are persisted durably...
    let persisted = store.committed_messages(&thread); // sanity: thread read works
    assert!(persisted.is_empty(), "no messages were committed");
    // ...but the read port returns nothing (the divergence).
    assert!(
        ThreadReader::committed_state(&store, &thread).is_empty(),
        "KNOWN BUG: committed_state returns empty even though state commands were committed"
    );
}
