//! T38-C03: storage capacity exhaustion on the SQLite commit authority.
//!
//! | Rule | Cause | Effect |
//! |---|---|---|
//! | X1 | growth capacity exhausted (bounded max_page_count seam) | commit_operation rejected; no commit/message/state row after reopen; database file stays bounded |
//! | X2 | read-only seam | same as X1 |
//! | X3 | capacity restored (reopen), same operation retried | applied exactly once; a further retry replays the ORIGINAL receipt |
//! | X4 | classification | capacity failure has its own class, distinct from a logical rejection (reproducer) |
//! | X5 | the single pooled connection is wedged | a second admission fails promptly with a classified error (reproducer: 24h acquire_timeout) |

use std::time::Duration;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::{Error, OperationCoordinator};
use awaken_agent_contract::thread::commit::operation::{
    CommitOperation, CommitOperationId, commit_payload_hash,
};
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_store_sqlite::SqliteCommitCoordinator;

const THREAD: &str = "capacity-thread";
const BOUNDED_FILE_BYTES: u64 = 8 * 1024 * 1024;

fn operation() -> CommitOperation {
    let commit = ThreadCommit {
        thread_id: ThreadId(THREAD.into()),
        run: RunDisposition::running(RunId("capacity-run".into())),
        messages: vec![Message::text(
            MessageId("capacity-message".into()),
            Role::Assistant,
            "x".repeat(1024 * 1024),
        )],
        state: Vec::new(),
        events: Vec::new(),
    };
    CommitOperation {
        operation_id: CommitOperationId::new(RunId("capacity-run".into()), 0),
        expected_thread_version: 0,
        payload_hash: commit_payload_hash(&commit).unwrap(),
        commit,
    }
}

async fn assert_nothing_committed(path: &str) {
    let reopened = SqliteCommitCoordinator::open(path).await.unwrap();
    assert_eq!(reopened.commit_count(), 0, "no commit row");
    assert!(
        reopened
            .committed_messages(&ThreadId(THREAD.into()))
            .is_empty(),
        "no message row"
    );
    assert!(
        reopened
            .committed_state(&ThreadId(THREAD.into()))
            .is_empty(),
        "no state row"
    );
    reopened.close().await;
}

/// X1/X2 + X3 for one failure seam.
async fn exhausted_then_restored(seam: &str) -> Error {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join(format!("{seam}.db"));
    let path = database.to_str().unwrap().to_string();
    SqliteCommitCoordinator::open(&path)
        .await
        .unwrap()
        .close()
        .await;

    let store = SqliteCommitCoordinator::open(&path).await.unwrap();
    match seam {
        "full" => store.exhaust_growth_capacity_for_test().await.unwrap(),
        _ => store.make_read_only_for_test().await.unwrap(),
    }
    let error = store
        .commit_operation(operation())
        .await
        .expect_err("exhausted capacity rejects the commit");
    assert_eq!(store.commit_count(), 0, "{seam}: fence unchanged");
    store.close().await;
    assert!(
        std::fs::metadata(&database).unwrap().len() < BOUNDED_FILE_BYTES,
        "{seam}: the seam bounds growth; no host disk filling"
    );
    assert_nothing_committed(&path).await;

    // X3: capacity restored by reopening (the seams are connection-local).
    let restored = SqliteCommitCoordinator::open(&path).await.unwrap();
    let applied = restored.commit_operation(operation()).await.unwrap();
    assert!(
        !applied.duplicate,
        "{seam}: first successful attempt applies"
    );
    let replay = restored.commit_operation(operation()).await.unwrap();
    assert!(replay.duplicate, "{seam}: retry is a replay");
    assert_eq!(
        (
            replay.commit_sequence,
            replay.thread_version,
            &replay.payload_hash
        ),
        (
            applied.commit_sequence,
            applied.thread_version,
            &applied.payload_hash
        ),
        "{seam}: original receipt"
    );
    assert_eq!(restored.commit_count(), 1, "{seam}: exactly once");
    assert_eq!(
        restored.committed_messages(&ThreadId(THREAD.into())).len(),
        1,
        "{seam}: one message"
    );
    restored.close().await;
    error
}

#[tokio::test]
async fn exhausted_capacity_rejects_atomically_and_retry_commits_once_with_original_receipt() {
    for seam in ["full", "read-only"] {
        let error = exhausted_then_restored(seam).await;
        assert!(
            !error.to_string().contains(&"x".repeat(64)),
            "{seam}: error does not echo payload"
        );
    }
}

#[tokio::test]
#[ignore = "product gap: coordinator Error has only Rejected(String); SQLITE_FULL/READONLY are not a distinct retryable capacity class"]
async fn exhausted_capacity_has_a_distinct_classification() {
    let error = exhausted_then_restored("full").await;
    assert!(
        !matches!(error, Error::Rejected(_)),
        "X4 capacity exhaustion must not share the logical-rejection class: {error:?}"
    );
}

#[tokio::test]
#[ignore = "product gap: awaken-sqlite-pool ADMISSION_WAIT is 24h, so a wedged connection masks exhaustion instead of failing admission"]
async fn wedged_single_connection_fails_admission_promptly() {
    let directory = tempfile::tempdir().unwrap();
    let pool = awaken_sqlite_pool::EmbeddedSqlite::file(directory.path().join("wedged.db"))
        .open()
        .await
        .unwrap();
    let _wedged = pool.acquire().await.unwrap();
    let second = tokio::time::timeout(Duration::from_secs(30), pool.acquire()).await;
    assert!(
        matches!(second, Ok(Err(_))),
        "X5 admission must fail with a classified error within a bounded wait, not hang"
    );
}
