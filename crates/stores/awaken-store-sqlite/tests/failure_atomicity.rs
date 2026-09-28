// @formal evidence checkpoint.crash_preserves_durable_truth
// @formal evidence run.commit_shape_atomicity
// @formal evidence thread_state.atomic_commit_and_replay
//! SQLite process-death and capacity-failure refinement tests.

use std::io::{Seek, SeekFrom, Write};
use std::time::Duration;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId};
use awaken_agent_contract::agent::state::{Command as StateCommand, MergePolicy, Scope};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::Coordinator;
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_reliability_testkit::CrashProcess;
use awaken_store_sqlite::SqliteCommitCoordinator;
use rusqlite::Connection;

const CRASH_CHILD: &str = "AWAKEN_SQLITE_CRASH_CHILD";
const DATABASE_PATH: &str = "AWAKEN_SQLITE_CRASH_DATABASE";
const READY_PATH: &str = "AWAKEN_SQLITE_CRASH_READY";
const CRASH_SEQUENCE: &str = "AWAKEN_SQLITE_CRASH_SEQUENCE";

fn path_text(path: &std::path::Path) -> Result<&str, std::io::Error> {
    path.to_str()
        .ok_or_else(|| std::io::Error::other("temporary SQLite path is not UTF-8"))
}

fn terminal_commit(text: String) -> ThreadCommit {
    ThreadCommit {
        thread_id: ThreadId("failure-thread".into()),
        run: RunDisposition::ended(RunId("failure-run".into()), EndCause::NaturalEnd),
        messages: vec![Message::text(
            MessageId("failure-message".into()),
            Role::Assistant,
            text,
        )],
        state: vec![
            StateCommand::set(
                Scope::Thread,
                MergePolicy::Disjoint,
                "failure.first",
                serde_json::json!(1),
            ),
            StateCommand::set(
                Scope::Thread,
                MergePolicy::Commutative,
                "failure.second",
                serde_json::json!({"complete": true}),
            ),
        ],
        events: Vec::new(),
    }
}

fn run_uncommitted_child_if_requested() -> Result<bool, Box<dyn std::error::Error>> {
    if std::env::var_os(CRASH_CHILD).is_none() {
        return Ok(false);
    }
    let database = std::env::var(DATABASE_PATH)?;
    let ready = std::env::var(READY_PATH)?;
    let sequence = std::env::var(CRASH_SEQUENCE)?.parse::<u64>()?;
    let connection = Connection::open(database)?;
    connection.execute_batch("PRAGMA journal_mode=WAL; BEGIN IMMEDIATE;")?;
    connection.execute(
        "INSERT INTO runtime_commit(sequence,thread_id,run_id,phase) VALUES (?1,?2,?3,'{}')",
        rusqlite::params![
            sequence,
            format!("partial-thread-{sequence}"),
            format!("partial-run-{sequence}")
        ],
    )?;
    let partial_thread = format!("partial-thread-{sequence}");
    let partial_state = serde_json::to_string(&StateCommand::set(
        Scope::Thread,
        MergePolicy::Disjoint,
        "partial.state",
        serde_json::json!(sequence),
    ))?;
    connection.execute(
        "INSERT INTO runtime_state_command(commit_sequence,thread_id,data) VALUES (?1,?2,?3)",
        rusqlite::params![sequence, partial_thread, partial_state],
    )?;
    std::fs::write(ready, b"transaction-open")?;
    std::thread::sleep(Duration::from_secs(60));
    Err("parent did not terminate SQLite crash child".into())
}

fn crash_uncommitted_child(
    test_name: &str,
    database: &std::path::Path,
    ready: &std::path::Path,
    sequence: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let status = CrashProcess::new(test_name, ready)
        .env(CRASH_CHILD, "1")
        .env(CRASH_SEQUENCE, sequence.to_string())
        .env(DATABASE_PATH, database)
        .env(READY_PATH, ready)
        .run()
        .map_err(std::io::Error::other)?;
    assert!(!status.success());
    Ok(())
}

#[tokio::test]
async fn uncommitted_transaction_is_rolled_back_after_process_kill()
-> Result<(), Box<dyn std::error::Error>> {
    // Cause/effect graph: C1=the schema is durable; C2=a separate process owns
    // BEGIN IMMEDIATE and has written both commit and state-command rows; C3=the
    // process dies before COMMIT; C4=a later complete two-command ThreadCommit
    // succeeds. Effects: E1=reopen observes neither partial row; E2=the later
    // commit obtains sequence one; E3=its exact ordered state batch survives a
    // second reopen. Decision table: R1=C1+C2+C3 -> E1; R2=R1+C4 -> E2+E3.
    // This exercises SQLite's actual WAL/rollback boundary rather than an
    // in-process error stub.
    if run_uncommitted_child_if_requested()? {
        return Ok(());
    }

    let directory = tempfile::tempdir()?;
    let database = directory.path().join("runtime.db");
    let ready = directory.path().join("transaction.ready");
    drop(SqliteCommitCoordinator::open(path_text(&database)?)?);

    crash_uncommitted_child(
        "uncommitted_transaction_is_rolled_back_after_process_kill",
        &database,
        &ready,
        1,
    )?;

    let store = SqliteCommitCoordinator::open(path_text(&database)?)?;
    assert_eq!(store.commit_count(), 0, "uncommitted row is invisible");
    assert!(
        store
            .committed_state(&ThreadId("partial-thread-1".into()))
            .is_empty(),
        "uncommitted state row is invisible"
    );
    let record = store.commit(terminal_commit("committed".into())).await?;
    assert_eq!(record.sequence, 1);
    drop(store);
    let reopened = SqliteCommitCoordinator::open(path_text(&database)?)?;
    assert_eq!(reopened.commit_count(), 1);
    let replayed = reopened.committed_state(&ThreadId("failure-thread".into()));
    assert_eq!(replayed.len(), 2, "complete state batch replays");
    assert_eq!(replayed[0].key.0, "failure.first", "commit order is stable");
    assert_eq!(
        replayed[1].key.0, "failure.second",
        "commit order is stable"
    );
    Ok(())
}

#[tokio::test]
async fn sqlite_read_only_rolls_back_the_complete_domain_commit()
-> Result<(), Box<dyn std::error::Error>> {
    // Cause/effect table: R1=query_only + a complete two-command ThreadCommit ->
    // SQLITE_READONLY and no commit, message, or state-command row after reopen.
    // This deterministic adapter error checks that a failure before durable
    // completion cannot publish any region of the staged aggregate.
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("read-only.db");
    let store = SqliteCommitCoordinator::open(path_text(&database)?)?;
    store.make_read_only_for_test()?;
    assert!(
        store
            .commit(terminal_commit("rejected".into()))
            .await
            .is_err()
    );
    drop(store);

    let reopened = SqliteCommitCoordinator::open(path_text(&database)?)?;
    assert_eq!(reopened.commit_count(), 0);
    assert!(
        reopened
            .committed_messages(&ThreadId("failure-thread".into()))
            .is_empty()
    );
    assert!(
        reopened
            .committed_state(&ThreadId("failure-thread".into()))
            .is_empty(),
        "failed domain commit publishes no state commands"
    );
    Ok(())
}

#[tokio::test]
async fn corrupt_uncommitted_wal_never_projects_a_partial_commit()
-> Result<(), Box<dyn std::error::Error>> {
    // Cause/effect table: R1=complete commit A -> durable message and both state
    // commands; R2=R1 + uncommitted commit/state rows B + process death + corrupt
    // WAL tail -> reopen either fails closed or exposes exactly A and none of B.
    // This distinguishes complete-prefix recovery from partial-state replay.
    if run_uncommitted_child_if_requested()? {
        return Ok(());
    }
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("corrupt-wal.db");
    let ready = directory.path().join("corrupt-wal.ready");
    let store = SqliteCommitCoordinator::open(path_text(&database)?)?;
    assert_eq!(
        store
            .commit(terminal_commit("durable-prefix".into()))
            .await?
            .sequence,
        1
    );
    drop(store);

    crash_uncommitted_child(
        "corrupt_uncommitted_wal_never_projects_a_partial_commit",
        &database,
        &ready,
        2,
    )?;

    let mut wal_name = database.as_os_str().to_os_string();
    wal_name.push("-wal");
    let wal = std::path::PathBuf::from(wal_name);
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&wal)?;
    let length = file.metadata()?.len();
    let offset = length.saturating_sub(16);
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(b"corrupt-wal-tail")?;
    file.sync_all()?;

    if let Ok(reopened) = SqliteCommitCoordinator::open(path_text(&database)?) {
        assert_eq!(reopened.commit_count(), 1);
        let messages = reopened.committed_messages(&ThreadId("failure-thread".into()));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text_content(), "durable-prefix");
        assert_eq!(
            reopened
                .committed_state(&ThreadId("failure-thread".into()))
                .len(),
            2,
            "only the complete durable state batch is visible"
        );
        assert!(
            reopened
                .committed_state(&ThreadId("partial-thread-2".into()))
                .is_empty(),
            "the corrupt uncommitted state tail is never projected"
        );
    }
    Ok(())
}

#[tokio::test]
async fn sqlite_full_rolls_back_the_complete_domain_commit()
-> Result<(), Box<dyn std::error::Error>> {
    // Cause/effect graph: C1=current database has no free growth pages;
    // C2=one staged commit carries a large message and two state commands.
    // Effect: SQLite reports FULL and every commit/message/state/run projection
    // remains absent after reopen. Decision rule R1=C1+C2 -> complete stutter.
    // A backend-specific capacity error therefore refines the common atomic
    // rejection result instead of exposing a partially committed Thread.
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("full.db");
    drop(SqliteCommitCoordinator::open(path_text(&database)?)?);

    let store = SqliteCommitCoordinator::open(path_text(&database)?)?;
    store.exhaust_growth_capacity_for_test()?;
    let result = store.commit(terminal_commit("x".repeat(1024 * 1024))).await;
    assert!(
        result.is_err(),
        "capacity exhaustion must reject the commit"
    );
    drop(store);

    let reopened = SqliteCommitCoordinator::open(path_text(&database)?)?;
    assert_eq!(reopened.commit_count(), 0);
    assert!(
        reopened
            .committed_messages(&ThreadId("failure-thread".into()))
            .is_empty()
    );
    assert!(
        reopened
            .committed_state(&ThreadId("failure-thread".into()))
            .is_empty(),
        "capacity failure publishes no state commands"
    );
    Ok(())
}
