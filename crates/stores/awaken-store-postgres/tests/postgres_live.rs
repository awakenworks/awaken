//! Live Postgres tests for the durable commit boundary.
//!
//! They run against a real Postgres (the commit transaction and projection
//! hydration cannot be exercised otherwise). The URL comes from
//! `AWAKEN_TEST_DATABASE_URL`, defaulting to the local dev container. If no
//! Postgres is reachable the test prints a skip notice and returns, so the suite
//! still passes on a machine without a database.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::state::{Command as StateCommand, MergePolicy, Scope};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::agent::waiting::{WaitingReason, WaitingTicket};
use awaken_agent_contract::audit::draft::Draft;
use awaken_agent_contract::audit::kind::Kind as EventKind;
use awaken_agent_contract::thread::commit::RunFact;
use awaken_agent_contract::thread::commit::coordinator::{Coordinator, Error as CommitError};
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::run_store::RunStore;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_store_postgres::PostgresCommitCoordinator;
use sqlx::Executor;
use sqlx::Row;
use sqlx::postgres::{PgPool, PgPoolOptions};

fn database_url() -> String {
    std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
    })
}

/// The URL with `search_path` pinned to `schema`, for the `connect()` test which
/// opens its own pool.
fn database_url_in_schema(schema: &str) -> String {
    let base = database_url();
    let sep = if base.contains('?') { '&' } else { '?' };
    format!("{base}{sep}options=-c%20search_path%3D{schema}")
}

/// A pool isolated to a fresh, empty Postgres schema. The production store takes
/// no table prefix (one runtime is one component); test isolation lives entirely
/// here, via `search_path`, and never leaks into the store's API. Returns `None`
/// (skip) when no Postgres is reachable.
async fn schema_pool(schema: &'static str) -> Option<PgPool> {
    let admin = match PgPool::connect(&database_url()).await {
        Ok(pool) => pool,
        Err(err) => {
            println!("[skip] no Postgres reachable: {err}");
            return None;
        }
    };
    let _ = admin
        .execute(format!("DROP SCHEMA IF EXISTS {schema} CASCADE").as_str())
        .await;
    admin
        .execute(format!("CREATE SCHEMA {schema}").as_str())
        .await
        .expect("create schema");
    admin.close().await;
    PgPoolOptions::new()
        .after_connect(move |conn, _meta| {
            Box::pin(async move {
                conn.execute(format!("SET search_path = {schema}").as_str())
                    .await?;
                Ok(())
            })
        })
        .connect(&database_url())
        .await
        .ok()
}

fn message(id: &str, text: &str) -> Message {
    Message {
        id: MessageId(id.to_string()),
        role: Role::Assistant,
        content: vec![ContentBlock::text(text)],
    }
}

fn ended(run: &str) -> RunFact {
    RunFact {
        run_id: RunId(run.to_string()),
        phase: Phase::Ended(EndCause::NaturalEnd),
    }
}

fn running(run: &str) -> RunFact {
    RunFact {
        run_id: RunId(run.to_string()),
        phase: Phase::Running,
    }
}

fn waiting_fact(run: &str) -> RunFact {
    RunFact {
        run_id: RunId(run.to_string()),
        phase: Phase::Waiting,
    }
}

fn ticket(run: &str, thread: &str) -> WaitingTicket {
    WaitingTicket {
        correlation_id: "corr-1".to_string(),
        run_id: RunId(run.to_string()),
        thread_id: ThreadId(thread.to_string()),
        snapshot_id: "snap-1".to_string(),
        catalog_fingerprint: "fp-1".to_string(),
        reason: WaitingReason::ToolPermission,
        call_id: Some("call-1".to_string()),
        pending_tool: None,
        deadline_ms: None,
    }
}

/// Build a coordinator over a fresh, isolated Postgres schema, or `None` (skip)
/// when no Postgres is reachable — the entry point every shared-conformance test
/// below uses so pg runs the SAME suite as the other backends.
async fn conformance_store(schema: &'static str) -> Option<PostgresCommitCoordinator> {
    let pool = schema_pool(schema).await?;
    Some(
        PostgresCommitCoordinator::with_pool(pool)
            .await
            .expect("coordinator"),
    )
}

// The shared store conformance suite (ADR-0039 2.6), run against the durable
// Postgres backend under skip-on-unreachable. Previously Postgres re-implemented a
// few cases by hand and OMITTED `events_ordered_and_paged` and `commits_accumulate`
// entirely; wiring the harness here closes that parity hole so Postgres cannot
// diverge from inmem/fs/sqlite on any covered behavior.

#[tokio::test]
async fn conformance_commit_then_read() {
    let Some(store) = conformance_store("t_c_read").await else {
        return;
    };
    awaken_store_conformance::commit_then_read(&store).await;
}

#[tokio::test]
async fn conformance_events_ordered_and_paged() {
    let Some(store) = conformance_store("t_c_events").await else {
        return;
    };
    awaken_store_conformance::events_ordered_and_paged(&store).await;
}

#[tokio::test]
async fn conformance_commits_accumulate() {
    let Some(store) = conformance_store("t_c_acc").await else {
        return;
    };
    awaken_store_conformance::commits_accumulate(&store).await;
}

#[tokio::test]
async fn conformance_terminal_run_is_fenced() {
    let Some(store) = conformance_store("t_c_fence").await else {
        return;
    };
    awaken_store_conformance::terminal_run_is_fenced(&store).await;
}

#[tokio::test]
async fn conformance_waiting_ticket_parks_then_clears() {
    let Some(store) = conformance_store("t_c_wait").await else {
        return;
    };
    awaken_store_conformance::waiting_ticket_parks_then_clears(&store).await;
}

#[tokio::test]
async fn conformance_concurrent_appends_are_dense_and_distinct() {
    let Some(store) = conformance_store("t_c_conc").await else {
        return;
    };
    awaken_store_conformance::concurrent_appends_are_dense_and_distinct(&store).await;
}

// Postgres keys by thread, so it isolates two threads in one store (as SQLite does).
#[tokio::test]
async fn conformance_two_threads_in_one_store_are_isolated() {
    let Some(store) = conformance_store("t_c_iso").await else {
        return;
    };
    awaken_store_conformance::two_threads_in_one_store_are_isolated(&store).await;
}

#[tokio::test]
async fn conformance_empty_store_reads_are_absent() {
    let Some(store) = conformance_store("t_c_empty").await else {
        return;
    };
    awaken_store_conformance::empty_store_reads_are_absent(&store).await;
}

// Postgres projects committed state commands back through the `committed_state`
// read port (rebuilt from the durable `runtime_state_command` rows), so a resumed
// run replays its accumulated state from durable truth — it runs the shared
// `committed_state_replays` conformance case (as inmem/fs/sqlite do).
#[tokio::test]
async fn conformance_committed_state_replays() {
    let Some(store) = conformance_store("t_c_state").await else {
        return;
    };
    awaken_store_conformance::committed_state_replays(&store).await;
}

// Durable state replay across a reconnect: committed state commands are served back
// through `committed_state` from a fresh coordinator over the same schema, proving
// the projection rebuilt from the durable `runtime_state_command` rows (not just an
// in-process advance).
#[tokio::test]
async fn reconnect_replays_committed_state() {
    let Some(pool) = schema_pool("t_c_state_reopen").await else {
        return;
    };
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
        let store = PostgresCommitCoordinator::with_pool(pool.clone())
            .await
            .expect("coordinator");
        store
            .commit(ThreadCommit {
                thread_id: thread.clone(),
                run_fact: running("r-state"),
                messages: vec![],
                state: commands.clone(),
                events: vec![],
                waiting: None,
            })
            .await
            .expect("commit state");
    }

    // The rows landed durably in the same transaction...
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runtime_state_command")
        .fetch_one(&pool)
        .await
        .expect("count state rows");
    assert_eq!(rows, 2, "both state commands persisted");

    // ...and a fresh coordinator rehydrates and serves them back through the port.
    let reopened = PostgresCommitCoordinator::with_pool(pool.clone())
        .await
        .expect("reconnect");
    assert_eq!(
        ThreadReader::committed_state(&reopened, &thread),
        commands,
        "committed state replays from durable truth after a reconnect"
    );
}

#[tokio::test]
async fn commit_persists_facts_messages_and_serves_reads() {
    let Some(pool) = schema_pool("t_commit").await else {
        return;
    };

    let coordinator = PostgresCommitCoordinator::with_pool(pool.clone())
        .await
        .expect("coordinator");

    let thread = ThreadId("thread-1".to_string());
    let commit = ThreadCommit {
        thread_id: thread.clone(),
        run_fact: ended("run-1"),
        messages: vec![message("m1", "hello"), message("m2", "world")],
        state: vec![StateCommand::set(
            Scope::Run,
            MergePolicy::Disjoint,
            "k",
            serde_json::json!("v"),
        )],
        events: vec![Draft {
            kind: EventKind::RunPhaseChanged,
            payload: serde_json::json!({"n": 1}),
        }],
        waiting: None,
    };

    let record = coordinator.commit(commit).await.expect("commit");
    assert_eq!(record.sequence, 1);
    assert_eq!(coordinator.commit_count(), 1);

    let run = RunStore::get(&coordinator, &RunId("run-1".to_string())).expect("run record");
    assert_eq!(run.phase, Phase::Ended(EndCause::NaturalEnd));
    assert_eq!(run.thread_id, thread);

    let messages = ThreadReader::committed_messages(&coordinator, &thread);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].id.0, "m1");

    // The state command and event rows persisted in the same transaction.
    let states: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runtime_state_command")
        .fetch_one(&pool)
        .await
        .expect("count states");
    assert_eq!(states, 1);
    let event_seq: i64 = sqlx::query("SELECT sequence FROM runtime_event")
        .fetch_one(&pool)
        .await
        .expect("event row")
        .get("sequence");
    assert_eq!(event_seq, 1_000); // first commit (1) * 1000 + offset 0
}

#[tokio::test]
async fn fence_increments_monotonically() {
    let Some(pool) = schema_pool("t_fence").await else {
        return;
    };
    let coordinator = PostgresCommitCoordinator::with_pool(pool.clone())
        .await
        .expect("coordinator");

    // A distinct run per commit: the fence increments across successive commits,
    // which is what this pins. (Re-committing one terminal run would trip the
    // terminal-is-final guard, which fences a stale owner's duplicate post-terminal
    // commit; a run ends exactly once.)
    for expected in 1..=3u64 {
        let record = coordinator
            .commit(ThreadCommit {
                thread_id: ThreadId("thread-1".to_string()),
                run_fact: ended(&format!("run-{expected}")),
                messages: vec![],
                state: vec![],
                events: vec![],
                waiting: None,
            })
            .await
            .expect("commit");
        assert_eq!(record.sequence, expected);
    }
    assert_eq!(coordinator.commit_count(), 3);
}

#[tokio::test]
async fn post_terminal_commit_is_fenced_durably() {
    // Terminal-is-final, fenced durably in the shared DB (the cross-node case): a
    // stale owner's duplicate commit over an already-`Ended` run is rejected by the
    // in-transaction `SELECT ... FOR UPDATE` on the run's `run_record` row, not by a
    // per-process projection. Exactly one terminal fact and no duplicate transcript
    // land in the database.
    let Some(pool) = schema_pool("t_terminal_final").await else {
        return;
    };
    let coordinator = PostgresCommitCoordinator::with_pool(pool.clone())
        .await
        .expect("coordinator");
    let thread = ThreadId("thread-1".to_string());
    let run = RunId("run-1".to_string());

    let commit = |fact: RunFact, msg_id: &str, text: &str| ThreadCommit {
        thread_id: thread.clone(),
        run_fact: fact,
        messages: vec![message(msg_id, text)],
        state: vec![],
        events: vec![],
        waiting: None,
    };

    // A mid-flight Running step, then the terminal Ended commit — both land.
    coordinator
        .commit(commit(running("run-1"), "m1", "step"))
        .await
        .expect("running step commits");
    coordinator
        .commit(commit(ended("run-1"), "m2", "all done"))
        .await
        .expect("first terminal commit lands");

    // A stale owner re-drives and tries to commit a duplicate over the now-terminal
    // run — terminal-is-final rejects it durably.
    let err = coordinator
        .commit(commit(ended("run-1"), "m3", "duplicate"))
        .await
        .expect_err("a post-terminal commit must be rejected");
    assert!(
        matches!(err, CommitError::Rejected(_)),
        "expected a Rejected error, got {err:?}"
    );

    // The shared DB carries exactly one terminal fact for the run and no duplicate
    // message; the rejected commit rolled back (and never advanced the fence).
    assert_eq!(
        coordinator.commit_count(),
        2,
        "the rejected commit did not advance the fence"
    );
    assert_eq!(
        RunStore::get(&coordinator, &run).map(|record| record.phase),
        Some(Phase::Ended(EndCause::NaturalEnd)),
        "the run stays terminal"
    );
    let commit_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM runtime_commit WHERE run_id = $1")
            .bind(&run.0)
            .fetch_one(&pool)
            .await
            .expect("count commit rows");
    assert_eq!(commit_rows, 2, "only the Running + Ended facts landed");
    let message_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runtime_message")
        .fetch_one(&pool)
        .await
        .expect("count message rows");
    assert_eq!(message_rows, 2, "the duplicate transcript never landed");
}

#[tokio::test]
async fn waiting_ticket_parks_then_clears() {
    let Some(pool) = schema_pool("t_waiting").await else {
        return;
    };
    let coordinator = PostgresCommitCoordinator::with_pool(pool.clone())
        .await
        .expect("coordinator");
    let run = RunId("run-1".to_string());

    coordinator
        .commit(ThreadCommit {
            thread_id: ThreadId("thread-1".to_string()),
            run_fact: waiting_fact("run-1"),
            messages: vec![],
            state: vec![],
            events: vec![],
            waiting: Some(ticket("run-1", "thread-1")),
        })
        .await
        .expect("park");
    assert!(ThreadReader::waiting_ticket(&coordinator, &run).is_some());

    coordinator
        .commit(ThreadCommit {
            thread_id: ThreadId("thread-1".to_string()),
            run_fact: ended("run-1"),
            messages: vec![],
            state: vec![],
            events: vec![],
            waiting: None,
        })
        .await
        .expect("resume to terminal");
    assert!(
        ThreadReader::waiting_ticket(&coordinator, &run).is_none(),
        "a terminal run clears its ticket (fail closed)"
    );
}

#[tokio::test]
async fn connect_applies_migrations_and_serves_a_commit() {
    let schema = "t_connect";
    if schema_pool(schema).await.is_none() {
        return;
    }

    let coordinator = PostgresCommitCoordinator::connect(&database_url_in_schema(schema))
        .await
        .expect("connect");
    coordinator
        .commit(ThreadCommit {
            thread_id: ThreadId("thread-1".to_string()),
            run_fact: ended("run-1"),
            messages: vec![],
            state: vec![],
            events: vec![],
            waiting: None,
        })
        .await
        .expect("commit");
    assert_eq!(coordinator.commit_count(), 1);
}

#[tokio::test]
async fn commit_maps_a_storage_failure_to_a_rejection() {
    let Some(pool) = schema_pool("t_fail").await else {
        return;
    };
    let coordinator = PostgresCommitCoordinator::with_pool(pool.clone())
        .await
        .expect("coordinator");

    // Remove a table the commit must write to, forcing the transaction to fail.
    sqlx::query("DROP TABLE runtime_commit")
        .execute(&pool)
        .await
        .expect("drop");

    let err = coordinator
        .commit(ThreadCommit {
            thread_id: ThreadId("thread-1".to_string()),
            run_fact: ended("run-1"),
            messages: vec![],
            state: vec![],
            events: vec![],
            waiting: None,
        })
        .await
        .expect_err("insert fails");
    assert!(matches!(
        err,
        awaken_agent_contract::thread::commit::coordinator::Error::Rejected(_)
    ));
}

#[tokio::test]
async fn projection_rehydrates_from_postgres_after_reconnect() {
    let Some(pool) = schema_pool("t_hydrate").await else {
        return;
    };

    {
        let coordinator = PostgresCommitCoordinator::with_pool(pool.clone())
            .await
            .expect("coordinator a");
        coordinator
            .commit(ThreadCommit {
                thread_id: ThreadId("thread-1".to_string()),
                run_fact: waiting_fact("run-1"),
                messages: vec![message("m1", "persisted")],
                state: vec![],
                events: vec![],
                waiting: Some(ticket("run-1", "thread-1")),
            })
            .await
            .expect("commit");
    } // coordinator A dropped — simulate a restart

    // A fresh coordinator on the same database hydrates committed truth.
    let restarted = PostgresCommitCoordinator::with_pool(pool.clone())
        .await
        .expect("coordinator b");
    assert_eq!(restarted.commit_count(), 1, "fence survives restart");
    let thread = ThreadId("thread-1".to_string());
    assert_eq!(
        ThreadReader::committed_messages(&restarted, &thread)[0]
            .id
            .0,
        "m1"
    );
    assert!(
        RunStore::get(&restarted, &RunId("run-1".to_string())).is_some(),
        "run record rehydrated"
    );
    assert!(
        ThreadReader::waiting_ticket(&restarted, &RunId("run-1".to_string())).is_some(),
        "active ticket rehydrated"
    );
}

/// Regression: concurrent commits must not collide on the commit sequence.
///
/// Two independent coordinators over one database model a two-node fleet — each has
/// its own in-memory projection, so allocating the sequence from that counter would
/// hand out the same value and every commit but one would fail on
/// `runtime_commit_pkey`. The sequence is allocated lock-free at the database via
/// `nextval` on a dedicated Postgres SEQUENCE, so a burst of concurrent commits —
/// across processes AND parallel within one — all succeed with distinct sequences
/// and never collide on the primary key. (The contract is distinctness, not
/// contiguity: a rolled-back allocation may leave a gap. This happy-path burst has
/// no rollbacks, so the distinct set is also contiguous 1..=n.)
#[tokio::test]
async fn concurrent_commits_get_distinct_sequences_no_pk_collision() {
    let Some(pool) = schema_pool("t_concurrent").await else {
        return;
    };
    let a = Arc::new(
        PostgresCommitCoordinator::with_pool(pool.clone())
            .await
            .expect("coordinator a"),
    );
    let b = Arc::new(
        PostgresCommitCoordinator::with_pool(pool.clone())
            .await
            .expect("coordinator b"),
    );

    let n: u32 = 16;
    let mut handles = Vec::new();
    for i in 0..n {
        // Alternate coordinators so half the burst commits through each independent
        // projection — the cross-process race the fix must close.
        let coord = if i % 2 == 0 { a.clone() } else { b.clone() };
        handles.push(tokio::spawn(async move {
            coord
                .commit(ThreadCommit {
                    thread_id: ThreadId(format!("thread-{i}")),
                    run_fact: ended(&format!("run-{i}")),
                    messages: vec![message(&format!("m{i}"), "x")],
                    state: Vec::new(),
                    events: Vec::new(),
                    waiting: None,
                })
                .await
        }));
    }

    let mut sequences = Vec::new();
    for h in handles {
        let record = h
            .await
            .expect("task joins")
            .expect("commit must not collide on the sequence primary key");
        sequences.push(record.sequence);
    }
    sequences.sort_unstable();
    let distinct: std::collections::BTreeSet<u64> = sequences.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        sequences.len(),
        "every concurrent commit got a DISTINCT sequence (no PK collision): {sequences:?}"
    );
    assert_eq!(sequences.len(), n as usize, "every commit succeeded");
    // No rollbacks here, so the distinct set is also contiguous.
    assert_eq!(
        sequences,
        (1..=u64::from(n)).collect::<Vec<_>>(),
        "a rollback-free burst allocates a contiguous 1..=n"
    );

    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runtime_commit")
        .fetch_one(&pool)
        .await
        .expect("count commits");
    assert_eq!(rows, i64::from(n), "all {n} commits persisted");
}
