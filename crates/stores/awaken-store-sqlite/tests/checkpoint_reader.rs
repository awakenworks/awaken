//! SQLite passes the shared store conformance suite, and a fresh instance over
//! the same database file resumes from committed facts (ADR-0039 2.5 / D4).

use awaken_agent_contract::agent::awaiting::{
    AwaitTarget, PauseReason, RemoteInputReason, ResumeTicket,
};
use awaken_agent_contract::agent::message::{Id as MsgId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::state::{Command as StateCommand, MergePolicy, Scope};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::draft::Draft;
use awaken_agent_contract::audit::kind::Kind as EventKind;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::Coordinator;
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::checkpoint::{CheckpointReader, EventScope};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::lifecycle::{
    RunLifecycleCursor, RunLifecycleEventKind, RunLifecycleFeed,
};
use awaken_agent_contract::thread::read::recovery::RunRecoverySource;
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
async fn conformance_transcript_snapshots_freeze_append_only_prefix() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    awaken_store_conformance::transcript_snapshots_freeze_append_only_prefix(&store).await;
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
async fn conformance_recovery_snapshot_is_consistent() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    awaken_store_conformance::recovery_snapshot_is_consistent(&store).await;
}

#[tokio::test]
async fn conformance_commit_operation_is_idempotent_and_cas() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    awaken_store_conformance::commit_operation_is_idempotent_and_cas(&store).await;
}

#[tokio::test]
async fn conformance_concurrent_operations_cas_one_winner() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    awaken_store_conformance::concurrent_operations_cas_one_winner(&store).await;
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
    use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;

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
        CommittedThreadView::committed_state(&store, &thread),
        commands,
        "committed state replays from durable truth after a reopen"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn lifecycle_feed_observes_peer_commits_and_backfills_exclusively() {
    let dir = std::env::temp_dir().join(format!(
        "awaken_store_sqlite_lifecycle_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("commit.db");
    let path = path.to_str().expect("utf8 path");
    let thread = ThreadId("lifecycle-thread".into());
    let run = RunId("lifecycle-run".into());
    let feed = SqliteCommitCoordinator::open(path).expect("open feed before peer");
    let writer = SqliteCommitCoordinator::open(path).expect("open peer writer");
    for disposition in [
        RunDisposition::running(run.clone()),
        RunDisposition::awaiting(ResumeTicket::new(
            "lifecycle-correlation",
            run.clone(),
            thread.clone(),
            "snapshot",
            "catalog",
            AwaitTarget::Pause(PauseReason::Manual),
        )),
        RunDisposition::running(run.clone()),
        RunDisposition::ended(run.clone(), EndCause::NaturalEnd),
    ] {
        writer
            .commit(ThreadCommit::assemble(
                thread.clone(),
                disposition,
                true,
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ))
            .await
            .expect("lifecycle commit");
    }

    let first = feed.events_after(RunLifecycleCursor(0), 2).await.unwrap();
    assert_eq!(
        first
            .events
            .iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>(),
        vec![
            RunLifecycleEventKind::Running,
            RunLifecycleEventKind::Awaiting
        ]
    );
    let second = feed.events_after(first.next_cursor, 10).await.unwrap();
    assert_eq!(
        second
            .events
            .iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>(),
        vec![
            RunLifecycleEventKind::Resumed,
            RunLifecycleEventKind::Completed
        ]
    );
    assert!(second.next_cursor > first.next_cursor);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn recovery_snapshot_observes_peer_committed_transcript_and_state() {
    let dir = std::env::temp_dir().join(format!(
        "awaken_store_sqlite_peer_recovery_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("commit.db");
    let path = path.to_str().expect("utf8 path");
    let thread = ThreadId("peer-recovery-thread".into());
    let run = RunId("peer-recovery-run".into());
    let reader = SqliteCommitCoordinator::open(path).expect("open reader before peer");
    let writer = SqliteCommitCoordinator::open(path).expect("open peer writer");
    writer
        .commit(ThreadCommit {
            thread_id: thread.clone(),
            run: RunDisposition::ended(run.clone(), EndCause::NaturalEnd),
            messages: vec![Message::text(
                MsgId("peer-response".into()),
                Role::Assistant,
                r#"{"result":"peer-visible"}"#,
            )],
            state: vec![StateCommand::set(
                Scope::Thread,
                MergePolicy::Disjoint,
                "usage",
                serde_json::json!({"input_tokens": 3}),
            )],
            events: Vec::new(),
        })
        .await
        .expect("peer commit");

    let snapshot = reader
        .recovery_snapshot(&thread, &run)
        .await
        .expect("authoritative peer snapshot");

    assert_eq!(snapshot.latest_run_id, Some(run));
    assert_eq!(snapshot.messages.len(), 1);
    assert_eq!(
        snapshot.messages[0].text_content(),
        r#"{"result":"peer-visible"}"#
    );
    assert_eq!(snapshot.state.len(), 1);
    assert_eq!(snapshot.thread_version, 1);
    assert_eq!(snapshot.next_commit_ordinal, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn open_wait_tracks_only_the_latest_run_across_reopen() {
    /* Cause/effect decision table for SQLite's thread-level awaiting boundary.
     * Causes: C1 a Run has a committed awaiting ticket; C2 a later Run exists on
     * the same thread; C3 that later Run is terminal; C4 the store is reopened and
     * its projection hydrated; C5 the latest Run is awaiting with a matching ticket.
     * Effects: E1 open_wait returns none; E2 open_wait returns the exact latest
     * Run/ticket. Rules: R1 C1+!C2 => E2; R2 C1+C2+C3+!C4 => E1; R3
     * C1+C2+C3+C4 => E1; R4 C1+C2+!C3+C5 => E2 for the new latest Run. This
     * prevents a historical tool wait from blocking a later turn.
     */
    let dir = std::env::temp_dir().join(format!(
        "awaken_store_sqlite_latest_wait_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("commit.db");
    let path = path.to_str().expect("utf8 path");
    let thread = ThreadId("latest-wait-thread".into());
    let old_run = RunId("old-awaiting-run".into());
    let old_ticket = ResumeTicket::new(
        "old-correlation",
        old_run.clone(),
        thread.clone(),
        "old-snapshot",
        "catalog",
        AwaitTarget::RemoteInput {
            reason: RemoteInputReason::UserInput,
            call_id: "old-call".into(),
        },
    );

    {
        let store = SqliteCommitCoordinator::open(path).expect("open");
        store
            .commit(ThreadCommit::assemble(
                thread.clone(),
                RunDisposition::awaiting(old_ticket.clone()),
                true,
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ))
            .await
            .expect("old awaiting commit");
        assert_eq!(
            store.open_wait_for_thread(&thread),
            Some((old_run.clone(), old_ticket.clone())),
            "R1: the awaiting Run is open while it remains latest"
        );

        store
            .commit(ThreadCommit::assemble(
                thread.clone(),
                RunDisposition::ended(RunId("new-terminal-run".into()), EndCause::NaturalEnd),
                true,
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ))
            .await
            .expect("new terminal commit");
        assert!(
            store.resume_ticket_for(&old_run).is_some(),
            "the historical ticket remains addressable by its Run"
        );
        assert_eq!(
            store.open_wait_for_thread(&thread),
            None,
            "R2: an older ticket cannot make a terminal latest Run appear awaiting"
        );
    }

    let reopened = SqliteCommitCoordinator::open(path).expect("reopen");
    assert_eq!(
        reopened.open_wait_for_thread(&thread),
        None,
        "R3: hydration preserves the latest-Run boundary"
    );

    let latest_run = RunId("latest-awaiting-run".into());
    let latest_ticket = ResumeTicket::new(
        "latest-correlation",
        latest_run.clone(),
        thread.clone(),
        "latest-snapshot",
        "catalog",
        AwaitTarget::RemoteInput {
            reason: RemoteInputReason::UserInput,
            call_id: "latest-call".into(),
        },
    );
    reopened
        .commit(ThreadCommit::assemble(
            thread.clone(),
            RunDisposition::awaiting(latest_ticket.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("latest awaiting commit");
    assert_eq!(
        reopened.open_wait_for_thread(&thread),
        Some((latest_run, latest_ticket)),
        "R4: the exact latest awaiting Run remains resumable"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
