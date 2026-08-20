//! Invariant 2 for the durable backend (ADR-0039 D4): a fresh instance over the
//! same directory replays committed facts and resumes; a torn final line is
//! discarded so recovery keeps exactly the committed prefix.

use std::fs::OpenOptions;
use std::io::Write;

use awaken_agent_contract::agent::message::{Id as MsgId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::draft::Draft;
use awaken_agent_contract::audit::kind::Kind as EventKind;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::{Coordinator, OperationCoordinator};
use awaken_agent_contract::thread::commit::operation::{
    CommitOperation, CommitOperationId, CommitPayloadHash,
};
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::checkpoint::{CheckpointReader, EventScope};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_store_fs::FsCommitCoordinator;

fn checkpoint(thread: &ThreadId, run: &RunId, text: &str) -> ThreadCommit {
    ThreadCommit {
        thread_id: thread.clone(),
        run: RunDisposition::ended(run.clone(), EndCause::NaturalEnd),
        messages: vec![Message::text(
            MsgId(format!("m-{text}")),
            Role::Assistant,
            text,
        )],
        state: Vec::new(),
        events: vec![Draft {
            kind: EventKind::RunStateChanged,
            payload: serde_json::Value::Null,
        }],
    }
}

#[tokio::test]
async fn reopen_replays_committed_facts() {
    // Test design — state-transition contract:
    // Given Empty --commit/ack--> Durable(A) --restart--> Recovered(A),
    // every acknowledged fact (message, terminal run, event) must be identical.
    let dir = std::env::temp_dir().join("awaken_store_fs_reopen");
    let _ = std::fs::remove_dir_all(&dir);
    let thread = ThreadId("t1".to_string());
    let run = RunId("r1".to_string());

    {
        let store = FsCommitCoordinator::open(&dir).await.expect("open");
        store
            .commit(checkpoint(&thread, &run, "hello"))
            .await
            .expect("commit");
        // store dropped here — simulate a process restart
    }

    let store = FsCommitCoordinator::open(&dir).await.expect("reopen");
    assert_eq!(store.committed_messages(&thread).len(), 1);
    assert_eq!(
        store.run(&run).map(|record| record.state),
        Some(RunState::Ended(EndCause::NaturalEnd)),
    );
    assert_eq!(store.list_events(&EventScope::Run(run), None, 10).len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn torn_final_line_is_discarded() {
    // Test design — crash matrix, crash point `write(JSON-prefix)` before newline:
    // Durable(A) + Volatile(torn B) --restart--> Durable(A), never partial B.
    let dir = std::env::temp_dir().join("awaken_store_fs_torn");
    let _ = std::fs::remove_dir_all(&dir);
    let thread = ThreadId("t1".to_string());
    let run = RunId("r1".to_string());

    {
        let store = FsCommitCoordinator::open(&dir).await.expect("open");
        store
            .commit(checkpoint(&thread, &run, "good"))
            .await
            .expect("commit");
    }
    // Simulate a crash mid-append: a partial, unparseable trailing record.
    {
        let mut file = OpenOptions::new()
            .append(true)
            .open(dir.join("commits.ndjson"))
            .expect("append");
        file.write_all(b"{\"thread_id\":\"t1\",\"run_fact\":")
            .expect("torn write");
    }

    let store = FsCommitCoordinator::open(&dir).await.expect("reopen");
    // Exactly the committed prefix survives; the torn tail is discarded.
    assert_eq!(store.committed_messages(&thread).len(), 1);
    assert!(store.run(&run).is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn recovery_truncates_a_torn_tail_before_acknowledging_a_new_commit() {
    // Test design — model-based recovery sequence:
    // Durable(A) + Torn(B) --recover/truncate--> Durable(A)
    // --commit/ack(C)--> Durable(A,C) --restart--> Recovered(A,C).
    // Invariant: an acknowledged C cannot remain hidden behind an old torn tail.
    let dir = std::env::temp_dir().join("awaken_store_fs_torn_then_commit");
    let _ = std::fs::remove_dir_all(&dir);
    let thread = ThreadId("t-torn-then-commit".to_string());
    let first_run = RunId("r-before-crash".to_string());
    let second_run = RunId("r-after-recovery".to_string());

    {
        let store = FsCommitCoordinator::open(&dir).await.expect("open");
        store
            .commit(checkpoint(&thread, &first_run, "before"))
            .await
            .expect("first commit");
    }
    {
        let mut file = OpenOptions::new()
            .append(true)
            .open(dir.join("commits.ndjson"))
            .expect("append torn tail");
        file.write_all(b"{\"incomplete\":")
            .expect("simulate interrupted append");
    }
    {
        let recovered = FsCommitCoordinator::open(&dir).await.expect("recover");
        assert_eq!(recovered.committed_messages(&thread).len(), 1);
        recovered
            .commit(checkpoint(&thread, &second_run, "after"))
            .await
            .expect("post-recovery commit is acknowledged");
    }

    let reopened = FsCommitCoordinator::open(&dir)
        .await
        .expect("second restart");
    let messages = reopened.committed_messages(&thread);
    assert_eq!(messages.len(), 2);
    assert!(reopened.run(&first_run).is_some());
    assert!(reopened.run(&second_run).is_some());
    let bytes = std::fs::read(dir.join("commits.ndjson")).expect("read durable log");
    assert!(
        !bytes
            .windows(b"incomplete".len())
            .any(|w| w == b"incomplete")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn newline_terminated_corruption_fails_closed() {
    // Test design — corruption classification:
    // only an unterminated final record is a legal crash artifact. A bad record
    // followed by a newline is durable/middle corruption, so recovery must error
    // instead of silently accepting a shorter history.
    let dir = std::env::temp_dir().join("awaken_store_fs_corrupt_record");
    let _ = std::fs::remove_dir_all(&dir);
    let thread = ThreadId("t-corrupt".to_string());
    let run = RunId("r-corrupt".to_string());

    {
        let store = FsCommitCoordinator::open(&dir).await.expect("open");
        store
            .commit(checkpoint(&thread, &run, "durable"))
            .await
            .expect("commit");
    }
    {
        let mut file = OpenOptions::new()
            .append(true)
            .open(dir.join("commits.ndjson"))
            .expect("append corruption");
        file.write_all(b"{not-json}\n")
            .expect("write complete corrupt record");
    }

    let error = FsCommitCoordinator::open(&dir)
        .await
        .err()
        .expect("durable corruption must fail closed");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn operation_receipt_survives_reopen() {
    // Test design — idempotency state machine:
    // Unseen(op) --commit/ack--> Applied(op, receipt) --restart/retry(op)-->
    // Duplicate(receipt), while the payload appears exactly once.
    let dir = std::env::temp_dir().join("awaken_store_fs_receipt_reopen");
    let _ = std::fs::remove_dir_all(&dir);
    let operation = CommitOperation {
        operation_id: CommitOperationId::new(RunId("receipt-run".into()), 0),
        expected_thread_version: 0,
        payload_hash: CommitPayloadHash("sha256:receipt".into()),
        commit: ThreadCommit {
            thread_id: ThreadId("receipt-thread".into()),
            run: RunDisposition::running(RunId("receipt-run".into())),
            messages: vec![Message::text(
                MsgId("receipt-message".into()),
                Role::Assistant,
                "once",
            )],
            state: Vec::new(),
            events: Vec::new(),
        },
    };
    {
        let store = FsCommitCoordinator::open(&dir).await.expect("open");
        assert!(
            !store
                .commit_operation(operation.clone())
                .await
                .unwrap()
                .duplicate
        );
    }
    let reopened = FsCommitCoordinator::open(&dir).await.expect("reopen");
    assert!(
        reopened
            .commit_operation(operation)
            .await
            .expect("durable duplicate receipt")
            .duplicate
    );
    assert_eq!(
        reopened
            .committed_messages(&ThreadId("receipt-thread".into()))
            .len(),
        1
    );
    let _ = std::fs::remove_dir_all(dir);
}
