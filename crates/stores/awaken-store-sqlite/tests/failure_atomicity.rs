//! SQLite process-death and capacity-failure refinement tests.

use std::io::{Seek, SeekFrom, Write};
use std::process::{Child, Command, Stdio};
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
        state: Vec::new(),
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
    std::fs::write(ready, b"transaction-open")?;
    std::thread::sleep(Duration::from_secs(60));
    Err("parent did not terminate SQLite crash child".into())
}

fn spawn_uncommitted_child(
    test_name: &str,
    database: &std::path::Path,
    ready: &std::path::Path,
    sequence: u64,
) -> Result<Child, Box<dyn std::error::Error>> {
    Ok(Command::new(std::env::current_exe()?)
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .env(CRASH_CHILD, "1")
        .env(CRASH_SEQUENCE, sequence.to_string())
        .env(DATABASE_PATH, database)
        .env(READY_PATH, ready)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?)
}

fn wait_for_open_transaction(ready: &std::path::Path) -> Result<(), std::io::Error> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    ready
        .exists()
        .then_some(())
        .ok_or_else(|| std::io::Error::other("child never reached the open transaction"))
}

#[tokio::test]
async fn uncommitted_transaction_is_rolled_back_after_process_kill()
-> Result<(), Box<dyn std::error::Error>> {
    // Cause/effect graph: C1=the schema is durable; C2=a separate process owns
    // BEGIN IMMEDIATE and has written a commit row; C3=the process dies before
    // COMMIT. Effects: E1=reopen observes no commit/projection fact; E2=a later
    // valid commit obtains sequence one. This exercises SQLite's actual WAL/
    // rollback boundary rather than an in-process error stub.
    if run_uncommitted_child_if_requested()? {
        return Ok(());
    }

    let directory = tempfile::tempdir()?;
    let database = directory.path().join("runtime.db");
    let ready = directory.path().join("transaction.ready");
    drop(SqliteCommitCoordinator::open(path_text(&database)?)?);

    let mut child = spawn_uncommitted_child(
        "uncommitted_transaction_is_rolled_back_after_process_kill",
        &database,
        &ready,
        1,
    )?;
    wait_for_open_transaction(&ready)?;
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
async fn sqlite_read_only_rolls_back_the_complete_domain_commit()
-> Result<(), Box<dyn std::error::Error>> {
    // Error-guessing plus atomicity oracle: PRAGMA query_only deterministically
    // produces SQLITE_READONLY on the same live connection. The attempted
    // multi-table domain commit must leave no durable commit or projection row.
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
    Ok(())
}

#[tokio::test]
async fn corrupt_uncommitted_wal_never_projects_a_partial_commit()
-> Result<(), Box<dyn std::error::Error>> {
    // Recovery-oracle design: one valid commit is the durable prefix; a second
    // process writes an uncommitted WAL frame, dies, and that tail is corrupted.
    // Reopen may recover the prefix or fail closed, but must never expose the
    // second fence or its partial domain rows.
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

    let mut child = spawn_uncommitted_child(
        "corrupt_uncommitted_wal_never_projects_a_partial_commit",
        &database,
        &ready,
        2,
    )?;
    wait_for_open_transaction(&ready)?;
    child.kill()?;
    assert!(!child.wait()?.success());

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
    }
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
