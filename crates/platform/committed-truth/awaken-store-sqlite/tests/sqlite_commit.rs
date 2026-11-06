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
    CommitOperation, CommitOperationId, CommitPayloadHash, CommitReceipt, commit_payload_hash,
};
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::recovery::RunRecoverySource;
use awaken_store_sqlite::SqliteCommitCoordinator;

/// A raw fixture connection with SQLite's plain defaults (file created when
/// missing, foreign keys off, journal mode untouched), outside the store policy.
async fn raw_sqlite(path: &std::path::Path) -> sqlx::SqlitePool {
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true)
                .foreign_keys(false),
        )
        .await
        .expect("open raw SQLite fixture connection")
}

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
    let store = SqliteCommitCoordinator::open_in_memory()
        .await
        .expect("open");
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
    let store = SqliteCommitCoordinator::open_in_memory()
        .await
        .expect("open");
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
    let store = SqliteCommitCoordinator::open_in_memory()
        .await
        .expect("open");
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
        let store = SqliteCommitCoordinator::open(&path).await.expect("open a");
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

    let restarted = SqliteCommitCoordinator::open(&path).await.expect("open b");
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
async fn corrupt_waiting_ticket_does_not_poison_restart_or_thread_recovery() {
    // Cause/effect graph: C1=an Awaiting ThreadCommit durably stores Run,
    // transcript, state, audit, and ResumeTicket; C2=only the waiting-row JSON
    // becomes semantically unreadable; C3=process restarts and requests one
    // repeatable recovery prefix. Effects: E1=store opens; E2=all non-ticket
    // durable facts and fences are preserved; E3=the damaged row exposes no
    // reply authority; E4=the row remains durable for canonical interruption to
    // settle through a later terminal ThreadCommit.
    //
    // | Rule | Run facts | Ticket row | Restart | Effect |
    // | CT1 | valid | valid | yes | ordinary complete recovery |
    // | CT2 | valid | corrupt | yes | E1+E2+E3+E4 |
    // | CT3 | corrupt | any | yes | fail closed outside this isolated repair |
    // Constraint: the adapter never deletes or rewrites the damaged waiting row;
    // it quarantines only its typed read projection. This case covers CT2.
    let directory = tempfile::tempdir().expect("temporary SQLite directory");
    let database = directory.path().join("corrupt-waiting-ticket.db");
    let thread = ThreadId("corrupt-ticket-thread".into());
    let run = RunId("corrupt-ticket-run".into());
    {
        let store = SqliteCommitCoordinator::open(database.to_str().unwrap())
            .await
            .expect("open");
        store
            .commit(ThreadCommit::assemble(
                thread.clone(),
                RunDisposition::awaiting(ticket(&run.0, &thread.0)),
                true,
                vec![message("corrupt-ticket-message", "retained transcript")],
                vec![StateCommand::set(
                    Scope::Run,
                    MergePolicy::Disjoint,
                    "test.recovery.fact",
                    serde_json::json!({"retained": true}),
                )],
                Vec::new(),
            ))
            .await
            .expect("CT2 commit complete Awaiting prefix");
    }
    let connection = raw_sqlite(&database).await;
    sqlx::query("UPDATE runtime_waiting SET ticket = ?1 WHERE run_id = ?2")
        .bind(r#"{"not":"a ResumeTicket"}"#)
        .bind(&run.0)
        .execute(&connection)
        .await
        .expect("isolate waiting ticket corruption");
    connection.close().await;

    let reopened = SqliteCommitCoordinator::open(database.to_str().unwrap())
        .await
        .expect("CT2/E1");
    assert!(
        CommittedThreadView::resume_ticket(&reopened, &run).is_none(),
        "CT2/E3 damaged ticket is not reply authority"
    );
    let snapshot = reopened
        .recovery_snapshot(&thread, &run)
        .await
        .expect("CT2/E2 recovery remains available");
    assert_eq!(snapshot.runs.len(), 1, "CT2/E2 Run fact");
    assert_eq!(snapshot.runs[0].state, RunState::Awaiting, "CT2/E2");
    assert_eq!(snapshot.messages.len(), 1, "CT2/E2 transcript");
    assert_eq!(snapshot.state.len(), 1, "CT2/E2 state");
    assert!(
        snapshot
            .events
            .iter()
            .any(|event| event.kind == EventKind::RunStateChanged),
        "CT2/E2 Awaiting audit"
    );
    assert!(snapshot.resume_tickets.is_empty(), "CT2/E3");
    assert_eq!(snapshot.thread_version, 1, "CT2/E2 fence");
    let inspector = raw_sqlite(&database).await;
    let retained: String =
        sqlx::query_scalar("SELECT ticket FROM runtime_waiting WHERE run_id = ?1")
            .bind(&run.0)
            .fetch_one(&inspector)
            .await
            .expect("CT2/E4 row retained");
    inspector.close().await;
    assert_eq!(retained, r#"{"not":"a ResumeTicket"}"#, "CT2/E4");
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
        let store = SqliteCommitCoordinator::open(database.to_str().unwrap())
            .await
            .expect("open");
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

    let reopened = SqliteCommitCoordinator::open(database.to_str().unwrap())
        .await
        .expect("reopen");
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
    let store = SqliteCommitCoordinator::open_in_memory()
        .await
        .expect("open");
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
    let store = SqliteCommitCoordinator::open_in_memory()
        .await
        .expect("open");

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
    // Cause/effect decision table: C1 a fresh operation carries its canonical
    // ThreadCommit hash; C2 its response is lost and the store reopens; C3 the
    // exact durable receipt exists; C4 the request names its original Thread.
    // R1 C1 + !C3 => one append and one receipt bound to the actual body.
    // R2 C2 + C3 + C4 => exact receipt replay before fresh body/CAS admission;
    // R3 C3 + !C4 => rejection with both complete prefixes unchanged. R2/R3
    // use the original shared warm/cold binding matrix, including stale/maximal
    // expected versions; historical declared hashes never prove body content.
    let path = std::env::temp_dir().join(format!(
        "awaken_store_sqlite_receipt_{}.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let commit = ThreadCommit {
        thread_id: ThreadId("receipt-thread".into()),
        run: RunDisposition::running(RunId("receipt-run".into())),
        messages: vec![message("receipt-message", "once")],
        state: Vec::new(),
        events: Vec::new(),
    };
    let operation = CommitOperation {
        operation_id: CommitOperationId::new(RunId("receipt-run".into()), 0),
        expected_thread_version: 0,
        payload_hash: commit_payload_hash(&commit).unwrap(),
        commit,
    };
    let applied = {
        let store = SqliteCommitCoordinator::open(path.to_str().unwrap())
            .await
            .expect("open");
        let receipt = store.commit_operation(operation.clone()).await.unwrap();
        assert!(!receipt.duplicate);
        assert_eq!(receipt.payload_hash, operation.payload_hash, "R1 body hash");
        receipt
    };
    let reopened = SqliteCommitCoordinator::open(path.to_str().unwrap())
        .await
        .expect("reopen");
    // Cold binding uses the same B1-B4 decision table as warm conformance.
    awaken_store_conformance::operation_receipt_replay_is_thread_bound(
        &reopened, &operation, &applied,
    )
    .await;
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

    // Legacy decision table: L1 a real plain commit supplies the original
    // record/Thread/Run/version/time; L2 its historical receipt row carries a
    // noncanonical declaration; L3 cold replay keeps that exact declaration
    // and Thread; L4 either declaration or Thread differs. L1+L2+L3 returns
    // the original receipt without append/restamp; L4 rejects atomically.
    // Only this historical test fixture inserts the old receipt, without
    // replacing a modern row or submitting a new fake-hash operation. This
    // preserves weak legacy observations, not proof of the original body.
    let legacy_commit = ThreadCommit {
        thread_id: ThreadId("legacy-receipt-thread".into()),
        run: RunDisposition::running(RunId("legacy-receipt-run".into())),
        messages: vec![message("legacy-receipt-message", "legacy once")],
        state: Vec::new(),
        events: Vec::new(),
    };
    let legacy_record = reopened.commit(legacy_commit.clone()).await.unwrap();
    let legacy_operation = CommitOperation {
        operation_id: CommitOperationId::new(RunId("legacy-receipt-run".into()), 0),
        expected_thread_version: 0,
        payload_hash: CommitPayloadHash("sha256:receipt".into()),
        commit: legacy_commit,
    };
    assert_ne!(
        legacy_operation.payload_hash,
        commit_payload_hash(&legacy_operation.commit).unwrap(),
        "L2 historical declaration is not a canonical body hash"
    );
    let legacy_prefix = reopened
        .recovery_snapshot(
            &legacy_operation.commit.thread_id,
            &legacy_operation.operation_id.run_id,
        )
        .await
        .unwrap();
    let legacy_receipt = CommitReceipt {
        operation_id: legacy_operation.operation_id.clone(),
        commit_sequence: legacy_record.sequence,
        thread_version: legacy_prefix.thread_version,
        payload_hash: legacy_operation.payload_hash.clone(),
        duplicate: false,
    };
    let raw = raw_sqlite(&path).await;
    sqlx::query(
        "INSERT INTO runtime_commit_receipt \
         (operation_run_id, operation_ordinal, thread_id, payload_hash, commit_sequence, thread_version) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(&legacy_receipt.operation_id.run_id.0)
    .bind(i64::try_from(legacy_receipt.operation_id.ordinal).unwrap())
    .bind(&legacy_operation.commit.thread_id.0)
    .bind(&legacy_receipt.payload_hash.0)
    .bind(i64::try_from(legacy_receipt.commit_sequence).unwrap())
    .bind(i64::try_from(legacy_receipt.thread_version).unwrap())
    .execute(&raw)
    .await
    .unwrap();
    raw.close().await;
    drop(reopened);
    let legacy_cold = SqliteCommitCoordinator::open(path.to_str().unwrap())
        .await
        .expect("reopen historical receipt");
    awaken_store_conformance::operation_receipt_replay_is_thread_bound(
        &legacy_cold,
        &legacy_operation,
        &legacy_receipt,
    )
    .await;
    let mut legacy_conflict = legacy_operation.clone();
    legacy_conflict.payload_hash = commit_payload_hash(&legacy_conflict.commit).unwrap();
    assert!(
        legacy_cold.commit_operation(legacy_conflict).await.is_err(),
        "L4 a canonical hash cannot replace the original declaration"
    );
    assert_eq!(
        legacy_cold
            .recovery_snapshot(
                &legacy_operation.commit.thread_id,
                &legacy_operation.operation_id.run_id,
            )
            .await
            .unwrap(),
        legacy_prefix,
        "L3/L4 complete prefix and original commit times remain unchanged"
    );
    assert_eq!(legacy_cold.commit_count(), 2, "L3/L4 no append");
    let raw = raw_sqlite(&path).await;
    let row: (String, i64, String, String, i64, i64) = sqlx::query_as(
        "SELECT operation_run_id, operation_ordinal, thread_id, payload_hash, commit_sequence, thread_version \
         FROM runtime_commit_receipt WHERE operation_run_id = ?1 AND operation_ordinal = ?2",
    )
    .bind(&legacy_receipt.operation_id.run_id.0)
    .bind(i64::try_from(legacy_receipt.operation_id.ordinal).unwrap())
    .fetch_one(&raw)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            legacy_receipt.operation_id.run_id.0,
            i64::try_from(legacy_receipt.operation_id.ordinal).unwrap(),
            legacy_operation.commit.thread_id.0,
            legacy_receipt.payload_hash.0,
            i64::try_from(legacy_receipt.commit_sequence).unwrap(),
            i64::try_from(legacy_receipt.thread_version).unwrap(),
        ),
        "L3/L4 the exact historical receipt row remains unchanged"
    );
    raw.close().await;
    let _ = std::fs::remove_file(path);
}
