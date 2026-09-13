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
async fn conformance_open_wait_selects_only_the_latest_run() {
    let store = SqliteCommitCoordinator::open_in_memory().expect("open");
    awaken_store_conformance::open_wait_selects_only_the_latest_run(&store).await;
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

    let original_times;
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
        // T1 commit then lose process cache => replay the same persisted
        // observation, not restart time; source facts and cursor remain equal.
        original_times = store
            .recovery_snapshot(&thread, &run)
            .await
            .unwrap()
            .commit_timestamps;
        assert_eq!(original_times.len(), 1);
        // store dropped — the in-memory projection is gone; only the DB file remains
    }

    let store = SqliteCommitCoordinator::open(path).expect("reopen");
    assert_eq!(
        store
            .recovery_snapshot(&thread, &run)
            .await
            .unwrap()
            .commit_timestamps,
        original_times,
        "T1"
    );
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
    // Cause/effect graph: C1 a peer writes commits 1..=5 after this reader opens;
    // C2 each commit owns one lifecycle event, including a claim-fenced
    // Rescheduled observation between Awaiting and resumed Running; C3 the feed
    // is paged at two.
    // Effects: E1 the stale process observes durable peer truth; E2 page kinds
    // classify Running/Awaiting/Resumed/Completed; E3 source commit cursors are
    // paired with encoded IDs [1000,2000] then [3000,4000,5000]; E4 the page
    // cursor remains the encoded event cursor; E5 Rescheduled does not consume
    // the preceding Awaiting state, so later Running remains Resumed.
    //
    // | Rule | C1 | C2 | C3 | Effects |
    // |---|---|---|---|---|
    // | S1 | T | T | F | E1,E2,E3,E5 |
    // | S2 | T | T | T | E1,E2,E3,E4,E5 |
    // Constraint/Invariant: the SQLite commit log, not the reader's in-memory
    // projection, owns peer lifecycle truth. Store open completes the mandatory
    // v10 migration first, so this query consumes only canonical RunStateChanged;
    // historical spelling normalization belongs exclusively to that migration.
    // Decision rule: S1 and S2 cover unpaged and paged reads, including exclusive
    // backfill and Resumed folding after canonicalization.
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
    let awaiting = RunDisposition::awaiting(ResumeTicket::new(
        "lifecycle-correlation",
        run.clone(),
        thread.clone(),
        "snapshot",
        "catalog",
        AwaitTarget::Pause(PauseReason::Manual),
    ));
    for disposition in [RunDisposition::running(run.clone()), awaiting.clone()] {
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
    writer
        .commit(ThreadCommit::rescheduled(thread.clone(), awaiting, 2))
        .await
        .expect("rescheduled lifecycle commit");
    for disposition in [
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
    assert_eq!(
        first
            .events
            .iter()
            .map(|event| (event.cursor.0, event.source_commit_cursor))
            .collect::<Vec<_>>(),
        vec![(1_000, 1), (2_000, 2)],
        "S2/E3 encoded/source domains"
    );
    let second = feed.events_after(first.next_cursor, 10).await.unwrap();
    assert_eq!(
        second
            .events
            .iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>(),
        vec![
            RunLifecycleEventKind::Rescheduled,
            RunLifecycleEventKind::Resumed,
            RunLifecycleEventKind::Completed
        ]
    );
    assert_eq!(
        second
            .events
            .iter()
            .map(|event| (event.cursor.0, event.source_commit_cursor))
            .collect::<Vec<_>>(),
        vec![(3_000, 3), (4_000, 4), (5_000, 5)],
        "S2/E3 encoded/source domains"
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
     * prevents a historical tool wait from blocking a later Run.
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

#[tokio::test]
async fn recovery_time_preserves_sqlite_milliseconds_and_timezone() {
    // D7 exact millisecond boundaries x UTC/offset spelling -> the same epoch
    // value. Seed the existing SQL observation column after a real commit;
    // reopening must read SQL truth, never a cached commit-clock value.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("precision.db");
    let store = SqliteCommitCoordinator::open(path.to_str().unwrap()).unwrap();
    let thread = ThreadId("precision".into());
    let run = RunId("precision".into());
    store
        .commit(ThreadCommit {
            thread_id: thread.clone(),
            run: RunDisposition::running(run.clone()),
            messages: vec![],
            state: vec![],
            events: vec![],
        })
        .await
        .unwrap();
    drop(store);
    let sql = rusqlite::Connection::open(&path).unwrap();
    for fraction in [0, 1, 999] {
        for (hour, offset) in [("00", "Z"), ("08", "+08:00")] {
            let observation = format!("1970-01-01T{hour}:00:01.{fraction:03}{offset}");
            sql.execute(
                "UPDATE runtime_commit SET committed_at = ?1",
                [&observation],
            )
            .unwrap();
            let reopened = SqliteCommitCoordinator::open(path.to_str().unwrap()).unwrap();
            let snapshot = reopened.recovery_snapshot(&thread, &run).await.unwrap();
            assert_eq!(
                snapshot.commit_timestamps.get(&1),
                Some(&(1_000 + fraction)),
                "D7 {observation}"
            );
        }
    }
}

#[tokio::test]
async fn recovery_scope_uses_committed_run_ownership_when_cache_is_damaged() {
    // C1 committed facts/tickets belong to two distinct Threads; C2 the derived
    // Run cache has wrong ownership or is absent; C3 a queried Thread has no
    // commits. R1 C1 => exact per-Thread snapshots; R2 C1+C2 => the same snapshots
    // from log-owned Run membership, never another Thread's events; R3 C3 =>
    // empty events/tickets. Cache damage is injected AFTER opening the store:
    // this proves recovery reads, not merely restart's compatibility hydration.
    // This read-only test makes no claim about accepting writes with a damaged cache.
    let path = std::env::temp_dir().join(format!(
        "awaken-sqlite-log-scope-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = SqliteCommitCoordinator::open(path.to_str().unwrap()).unwrap();
    let mut snapshots = Vec::new();
    for i in 0..2 {
        let thread = ThreadId(format!("scope-thread-{i}"));
        let run = RunId(format!("scope-run-{i}"));
        let ticket = ResumeTicket::new(
            format!("scope-correlation-{i}"),
            run.clone(),
            thread.clone(),
            "snapshot",
            "catalog",
            AwaitTarget::Pause(PauseReason::Manual),
        );
        store
            .commit(ThreadCommit::assemble(
                thread.clone(),
                RunDisposition::awaiting(ticket),
                true,
                vec![Message::text(
                    MsgId(format!("scope-message-{i}")),
                    Role::Assistant,
                    format!("owner-{i}"),
                )],
                vec![],
                vec![],
            ))
            .await
            .unwrap();
    }
    for i in 0..2 {
        snapshots.push(
            store
                .recovery_snapshot(
                    &ThreadId(format!("scope-thread-{i}")),
                    &RunId(format!("scope-run-{i}")),
                )
                .await
                .unwrap(),
        );
    }
    let conn = rusqlite::Connection::open(&path).unwrap();
    for mutation in [
        "UPDATE runtime_run_record SET thread_id=CASE WHEN thread_id='scope-thread-0' THEN 'scope-thread-1' ELSE 'scope-thread-0' END",
        "DELETE FROM runtime_run_record",
    ] {
        conn.execute(mutation, []).unwrap();
        for (i, expected) in snapshots.iter().enumerate() {
            let actual = store
                .recovery_snapshot(
                    &ThreadId(format!("scope-thread-{i}")),
                    &RunId(format!("scope-run-{i}")),
                )
                .await
                .unwrap();
            assert_eq!(
                &actual, expected,
                "R2 damaged cache cannot change committed scope"
            );
        }
        let empty = store
            .recovery_snapshot(
                &ThreadId("absent-thread".into()),
                &RunId("absent-run".into()),
            )
            .await
            .unwrap();
        assert!(
            empty.runs.is_empty() && empty.events.is_empty() && empty.resume_tickets.is_empty(),
            "R3"
        );
    }
    drop(conn);
    drop(store);
    std::fs::remove_file(path).unwrap();
}
