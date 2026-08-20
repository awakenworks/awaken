//! SQLite process-death and capacity-failure refinement tests.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::Coordinator;
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_store_sqlite::SqliteCommitCoordinator;
use rusqlite::Connection;

const CRASH_CHILD: &str = "AWAKEN_SQLITE_CRASH_CHILD";
const DATABASE_PATH: &str = "AWAKEN_SQLITE_CRASH_DATABASE";
const READY_PATH: &str = "AWAKEN_SQLITE_CRASH_READY";

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
        state: Vec::new(),
        events: Vec::new(),
    }
}

#[tokio::test]
async fn uncommitted_transaction_is_rolled_back_after_process_kill()
-> Result<(), Box<dyn std::error::Error>> {
    // Cause/effect graph: C1=the schema is durable; C2=a separate process owns
    // BEGIN IMMEDIATE and has written a commit row; C3=the process dies before
    // COMMIT. Effects: E1=reopen observes no commit/projection fact; E2=a later
    // valid commit obtains sequence one. This exercises SQLite's actual WAL/
    // rollback boundary rather than an in-process error stub.
    if std::env::var_os(CRASH_CHILD).is_some() {
        let database = std::env::var(DATABASE_PATH)?;
        let ready = std::env::var(READY_PATH)?;
        let connection = Connection::open(database)?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             BEGIN IMMEDIATE;
             INSERT INTO runtime_commit(sequence,thread_id,run_id,phase)
             VALUES (1,'partial-thread','partial-run','{}');",
        )?;
        std::fs::write(ready, b"transaction-open")?;
        std::thread::sleep(Duration::from_secs(60));
        return Err("parent did not terminate SQLite crash child".into());
    }

    let directory = tempfile::tempdir()?;
    let database = directory.path().join("runtime.db");
    let ready = directory.path().join("transaction.ready");
    drop(SqliteCommitCoordinator::open(path_text(&database)?)?);

    let mut child = Command::new(std::env::current_exe()?)
        .arg("--exact")
        .arg("uncommitted_transaction_is_rolled_back_after_process_kill")
        .arg("--nocapture")
        .env(CRASH_CHILD, "1")
        .env(DATABASE_PATH, &database)
        .env(READY_PATH, &ready)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(ready.exists(), "child never reached the open transaction");
    child.kill()?;
    assert!(!child.wait()?.success());

    let store = SqliteCommitCoordinator::open(path_text(&database)?)?;
    assert_eq!(store.commit_count(), 0, "uncommitted row is invisible");
    let record = store.commit(terminal_commit("committed".into())).await?;
    assert_eq!(record.sequence, 1);
    drop(store);
    assert_eq!(
        SqliteCommitCoordinator::open(path_text(&database)?)?.commit_count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn sqlite_full_rolls_back_the_complete_domain_commit()
-> Result<(), Box<dyn std::error::Error>> {
    // Cause/effect graph: C1=current database has no free growth pages;
    // C2=one staged commit needs additional pages. Effect: SQLite reports FULL
    // and every commit/message/run projection remains absent after reopen.
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
    Ok(())
}
