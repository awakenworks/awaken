//! Tests for the SQLite commit boundary. SQLite is embedded, so these always
//! run (no external server, no skip): an in-memory database covers commit, the
//! fence, reads, and awaiting tickets; a temp file covers rehydration on restart.

use awaken_agent_contract::agent::awaiting::{AwaitTarget, PauseReason, ResumeTicket};
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::state::{Command as StateCommand, MergePolicy, Scope};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::draft::Draft;
use awaken_agent_contract::audit::kind::Kind as EventKind;
use awaken_agent_contract::audit::run_event::RunEvent;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::{Coordinator, OperationCoordinator};
use awaken_agent_contract::thread::commit::operation::{
    CommitOperation, CommitOperationId, CommitPayloadHash,
};
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::recovery::RunRecoverySource;
use awaken_store_sqlite::SqliteCommitCoordinator;

fn message(id: &str, text: &str) -> Message {
    Message {
        id: MessageId(id.to_string()),
        role: Role::Assistant,
        content: vec![ContentBlock::text(text)],
    }
}

fn ended(run: &str) -> RunDisposition {
    RunDisposition::ended(RunId(run.to_string()), EndCause::NaturalEnd)
}

fn ticket(run: &str, thread: &str) -> ResumeTicket {
    ResumeTicket::new(
        "corr-1",
        RunId(run.to_string()),
        ThreadId(thread.to_string()),
        "snap-1",
        "fp-1",
        AwaitTarget::Pause(PauseReason::Manual),
    )
}

fn empty_commit(thread: &str, run: RunDisposition) -> ThreadCommit {
    ThreadCommit {
        thread_id: ThreadId(thread.to_string()),
        run,
        messages: vec![],
        state: vec![],
        events: vec![],
    }
}

#[tokio::test]
async fn commit_persists_facts_messages_and_serves_reads() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    let thread = ThreadId("thread-1".to_string());

    let commit = ThreadCommit {
        thread_id: thread.clone(),
        run: ended("run-1"),
        messages: vec![message("m1", "hello"), message("m2", "world")],
        state: vec![StateCommand::set(
            Scope::Run,
            MergePolicy::Disjoint,
            "k",
            serde_json::json!("v"),
        )],
        events: vec![Draft {
            kind: EventKind::RunStateChanged,
            payload: serde_json::json!({"n": 1}),
        }],
    };

    let record = store.commit(commit).await.expect("commit");
    assert_eq!(record.sequence, 1);
    assert_eq!(store.commit_count(), 1);

    let run = CommittedThreadView::run(&store, &RunId("run-1".to_string())).expect("run record");
    assert_eq!(run.state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(run.thread_id, thread);

    let messages = CommittedThreadView::committed_messages(&store, &thread);
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
            .commit(empty_commit("thread-1", ended(&format!("run-{expected}"))))
            .await
            .expect("commit");
        assert_eq!(record.sequence, expected);
    }
    assert_eq!(store.commit_count(), 3);
}

#[tokio::test]
async fn resume_ticket_awaits_then_clears() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    let run = RunId("run-1".to_string());

    store
        .commit(empty_commit(
            "thread-1",
            RunDisposition::awaiting(ticket("run-1", "thread-1")),
        ))
        .await
        .expect("await");
    assert!(CommittedThreadView::resume_ticket(&store, &run).is_some());

    store
        .commit(empty_commit("thread-1", ended("run-1")))
        .await
        .expect("resume to terminal");
    assert!(
        CommittedThreadView::resume_ticket(&store, &run).is_none(),
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
                run: RunDisposition::awaiting(ticket("run-1", "thread-1")),
                messages: vec![message("m1", "persisted")],
                state: vec![],
                events: vec![],
            })
            .await
            .expect("commit");
    } // store dropped — simulate a restart

    let restarted = SqliteCommitCoordinator::open(&path).expect("open b");
    assert_eq!(restarted.commit_count(), 1, "the fence survives restart");
    assert_eq!(
        CommittedThreadView::committed_messages(&restarted, &ThreadId("thread-1".to_string()))[0]
            .id
            .0,
        "m1"
    );
    assert!(CommittedThreadView::run(&restarted, &RunId("run-1".to_string())).is_some());
    assert!(
        CommittedThreadView::resume_ticket(&restarted, &RunId("run-1".to_string())).is_some(),
        "the active ticket rehydrated"
    );
    let snapshot = restarted
        .recovery_snapshot(
            &ThreadId("thread-1".to_string()),
            &RunId("run-1".to_string()),
        )
        .await
        .expect("recovery snapshot");
    assert_eq!(snapshot.thread_version, 1);
    assert_eq!(snapshot.store_cursor, 1);
    assert_eq!(snapshot.next_commit_ordinal, 1);
    assert_eq!(snapshot.messages.len(), 1);
    assert_eq!(snapshot.runs.len(), 1);
    assert_eq!(snapshot.resume_tickets.len(), 1);

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn consumed_resume_ticket_and_applied_receipt_rehydrate_atomically() {
    // Cause/effect graph: C1 an Awaiting Run owns ticket T; C2 one accepted
    // reply commits Running plus ResumeApplied(O1) in the same ThreadCommit; C3
    // the process/store is reopened. Effects: E1 T is absent; E2 O1 remains;
    // E3 both facts share one recovered Thread prefix/version.
    //
    // | Rule | Await | Resume commit | Reopen | Effect |
    // |---|---|---|---|---|
    // | SR1 | T active | Running + O1 | no | T absent, O1 present |
    // | SR2 | T active | Running + O1 | yes | E1 + E2 + E3 |
    //
    // Constraint/invariant: the receipt is an audit fact in the canonical
    // Thread commit, not a Session/UI recovery row that needs reconciliation.
    let directory = tempfile::tempdir().expect("temporary SQLite directory");
    let database = directory.path().join("resume-receipt.db");
    let thread = ThreadId("receipt-restart-thread".into());
    let run = RunId("receipt-restart-run".into());
    let operation_id = "managed-tool-reply-operation-1";
    let correlation_id = "managed-tool-reply-correlation-1";
    {
        let store = SqliteCommitCoordinator::open(database.to_str().unwrap()).expect("open");
        let active_ticket = ResumeTicket::new(
            correlation_id,
            run.clone(),
            thread.clone(),
            "receipt-restart-snapshot",
            "receipt-restart-catalog",
            AwaitTarget::Pause(PauseReason::Manual),
        );
        store
            .commit(ThreadCommit::assemble(
                thread.clone(),
                RunDisposition::awaiting(active_ticket),
                true,
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ))
            .await
            .expect("commit Awaiting ticket");
        store
            .commit(ThreadCommit::assemble(
                thread.clone(),
                RunDisposition::running(run.clone()),
                true,
                Vec::new(),
                Vec::new(),
                vec![
                    RunEvent::ResumeApplied {
                        operation_id: operation_id.into(),
                        correlation_id: correlation_id.into(),
                    }
                    .into(),
                ],
            ))
            .await
            .expect("atomically consume ticket and commit receipt");
    }

    let reopened = SqliteCommitCoordinator::open(database.to_str().unwrap()).expect("reopen");
    let snapshot = reopened
        .recovery_snapshot(&thread, &run)
        .await
        .expect("recover receipt prefix");
    assert!(snapshot.resume_tickets.is_empty(), "SR2/E1");
    let receipts = snapshot
        .events
        .iter()
        .filter(|event| event.kind == EventKind::ResumeApplied)
        .collect::<Vec<_>>();
    assert_eq!(receipts.len(), 1, "SR2/E2");
    assert_eq!(
        receipts[0].payload,
        serde_json::json!({
            "operation_id": operation_id,
            "correlation_id": correlation_id,
        }),
        "SR2/E2"
    );
    assert_eq!(snapshot.thread_version, 2, "SR2/E3");
    assert_eq!(snapshot.next_commit_ordinal, 2, "SR2/E3");
}

// G13: before any commit the projection has no truth — no partial state is
// pre-visible from a staged but uncommitted plan.
#[tokio::test]
async fn g13_projection_absent_before_commit() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    assert!(
        CommittedThreadView::run(&store, &RunId("run-1".to_string())).is_none(),
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
            run: ended("run-1"),
            messages: vec![],
            state: vec![],
            events: vec![],
        })
        .await;
    assert!(err.is_err(), "invalid plan must be rejected");

    assert!(
        CommittedThreadView::run(&store, &RunId("run-1".to_string())).is_none(),
        "no partial state after failed commit"
    );
    assert_eq!(
        store.commit_count(),
        0,
        "fence unchanged after rejected commit"
    );
}

#[tokio::test]
async fn operation_receipt_survives_reopen() {
    let path = std::env::temp_dir().join(format!(
        "awaken_store_sqlite_receipt_{}.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let operation = CommitOperation {
        operation_id: CommitOperationId::new(RunId("receipt-run".into()), 0),
        expected_thread_version: 0,
        payload_hash: CommitPayloadHash("sha256:receipt".into()),
        commit: ThreadCommit {
            thread_id: ThreadId("receipt-thread".into()),
            run: RunDisposition::running(RunId("receipt-run".into())),
            messages: vec![message("receipt-message", "once")],
            state: Vec::new(),
            events: Vec::new(),
        },
    };
    {
        let store = SqliteCommitCoordinator::open(path.to_str().unwrap()).expect("open");
        assert!(
            !store
                .commit_operation(operation.clone())
                .await
                .unwrap()
                .duplicate
        );
    }
    let reopened = SqliteCommitCoordinator::open(path.to_str().unwrap()).expect("reopen");
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
    let _ = std::fs::remove_file(path);
}
