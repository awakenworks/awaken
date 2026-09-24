//! Live Postgres tests for the durable commit boundary.
//!
//! They run against a real Postgres (the commit transaction and scoped recovery
//! reads cannot be exercised otherwise). The URL comes from
//! `AWAKEN_TEST_DATABASE_URL`, defaulting to the local dev container. If no
//! Postgres is reachable the test prints a skip notice and returns, so the suite
//! still passes on a machine without a database.

use std::sync::Arc;

use awaken_agent_contract::agent::awaiting::{
    AwaitTarget, PauseReason, PendingTool, ResumeTicket, ToolAwaitReason,
};
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::state::{Command as StateCommand, MergePolicy, Scope};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::draft::Draft;
use awaken_agent_contract::audit::kind::Kind as EventKind;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::{
    Coordinator, Error as CommitError, OperationCoordinator,
};
use awaken_agent_contract::thread::commit::operation::{
    CommitOperation, CommitOperationId, CommitPayloadHash,
};
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::checkpoint::{CheckpointReader, EventScope};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::recovery::RunRecoverySource;
use awaken_agent_contract::{RunLifecycleCursor, RunLifecycleEventKind, RunLifecycleFeed};
use awaken_store_postgres::PostgresCommitCoordinator;
use sqlx::Executor;
use sqlx::Row;
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::types::Json;

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

fn ended(run: &str) -> RunDisposition {
    RunDisposition::ended(RunId(run.to_string()), EndCause::NaturalEnd)
}

fn running(run: &str) -> RunDisposition {
    RunDisposition::running(RunId(run.to_string()))
}

fn awaiting_disposition(run: &str) -> RunDisposition {
    RunDisposition::awaiting(ticket(run, "thread-1"))
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

#[tokio::test]
async fn cold_event_reader_queries_committed_scopes_and_fails_closed() {
    // Cause/effect decision table: C1 cold reader after a peer commits two
    // Threads; C2 scope All/Thread/Run and an exclusive cursor; C3 cursor beyond
    // SQL BIGINT or unavailable pool. R1=C1+C2 -> E1 exact ordered committed
    // partition and E2 cursor excludes its predecessor; R2=C3 -> E3 error,
    // never an indistinguishable empty page. The SQL store remains the owner.
    let Some(pool) = schema_pool("t_pg_cold_event_reader").await else {
        return;
    };
    let writer = PostgresCommitCoordinator::with_pool(pool.clone())
        .await
        .expect("writer");
    for (thread, run) in [("cold-a", "cold-run-a"), ("cold-b", "cold-run-b")] {
        writer
            .commit(ThreadCommit::assemble(
                ThreadId(thread.into()),
                ended(run),
                true,
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ))
            .await
            .expect("commit");
    }
    let cold = PostgresCommitCoordinator::with_existing_pool(pool.clone())
        .await
        .expect("cold reader");
    let all = cold
        .list_events(&EventScope::All, None, 10)
        .await
        .expect("R1 all");
    assert_eq!(all.len(), 2, "R1/E1");
    assert!(all[0].sequence < all[1].sequence, "R1/E1 order");
    let thread = cold
        .list_events(&EventScope::Thread(ThreadId("cold-a".into())), None, 10)
        .await
        .expect("R1 thread");
    assert_eq!(thread, all[..1], "R1/E1 thread partition");
    let run = cold
        .list_events(&EventScope::Run(RunId("cold-run-b".into())), None, 10)
        .await
        .expect("R1 run");
    assert_eq!(run, all[1..], "R1/E1 run partition");
    let page = cold
        .list_events(&EventScope::All, Some(all[0].sequence), 10)
        .await
        .expect("R1 exclusive page");
    assert_eq!(page, all[1..], "R1/E2");
    assert!(
        cold.list_events(&EventScope::All, Some(i64::MAX as u64 + 1), 10)
            .await
            .is_err(),
        "R2/E3 out-of-range cursor"
    );
    pool.close().await;
    assert!(
        cold.list_events(&EventScope::All, None, 10).await.is_err(),
        "R2/E3 closed pool"
    );
}

#[tokio::test]
#[ignore = "million-fact PostgreSQL startup and scoped-open performance observation"]
async fn million_historical_facts_do_not_gate_startup_or_unrelated_thread_open() {
    // Causes: C1 over one million committed Message rows belong to one old
    // Thread; C2 a Coordinator restarts; C3 an unrelated Thread opens for
    // execution. Effects: E1 startup succeeds without full-history allocation;
    // E2 the old Thread still has a durable fact and its messages remain in SQL;
    // E3 the unrelated execution view contains only its own prefix. Rules:
    // M1=C1+C2 -> E1+E2, M2=C1+C2+C3 -> E1+E2+E3. The printed timings describe
    // this local primary and cannot establish a production restart SLO.
    let Some(pool) = schema_pool("t_pg_million_history").await else {
        panic!("explicit performance run requires PostgreSQL");
    };
    let writer = PostgresCommitCoordinator::with_pool(pool.clone())
        .await
        .expect("writer");
    writer
        .commit(ThreadCommit::assemble(
            ThreadId("old-thread".into()),
            ended("old-run"),
            true,
            vec![message("old-first", "historic")],
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("old commit");
    writer
        .commit(ThreadCommit::assemble(
            ThreadId("active-thread".into()),
            ended("active-run"),
            true,
            vec![message("active-first", "recent")],
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("active commit");
    sqlx::query(
        "INSERT INTO runtime_message (commit_sequence, thread_id, data) \
         SELECT 1, 'old-thread', $1::jsonb \
         FROM generate_series(1, 1000001)",
    )
    .bind(Json(message("old-bulk", "historic")))
    .execute(&pool)
    .await
    .expect("historical fact fixture");
    drop(writer);

    let started = std::time::Instant::now();
    let restarted = PostgresCommitCoordinator::with_existing_pool(pool.clone())
        .await
        .expect("M1/E1 restart above former bound");
    let startup = started.elapsed();
    assert_eq!(restarted.commit_count(), 2, "M1/E2 durable commit fence");
    let old_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM runtime_message WHERE thread_id = 'old-thread'")
            .fetch_one(&pool)
            .await
            .expect("M1/E2 old SQL facts");
    assert_eq!(old_count, 1_000_002, "M1/E2");

    let started = std::time::Instant::now();
    let active = restarted
        .open_thread_projection(&ThreadId("active-thread".into()))
        .await
        .expect("M2/E3 scoped open");
    let scoped_open = started.elapsed();
    assert_eq!(
        CommittedThreadView::committed_messages(&active, &ThreadId("active-thread".into())).len(),
        1,
        "M2/E3"
    );
    assert!(
        CommittedThreadView::committed_messages(&active, &ThreadId("old-thread".into())).is_empty(),
        "M2/E3 unrelated history absent"
    );
    println!(
        "million_history startup_ms={:.3} scoped_open_ms={:.3} facts={old_count}",
        startup.as_secs_f64() * 1000.0,
        scoped_open.as_secs_f64() * 1000.0
    );
}

#[tokio::test]
async fn scoped_execution_view_rejects_another_thread_before_writing() {
    // C1 a view was opened for Thread A; C2 a caller submits Thread B's commit.
    // R1=C1+C2 -> E1 rejection before SQL, E2 no B fact and A remains intact.
    let Some(pool) = schema_pool("t_pg_scoped_view").await else {
        return;
    };
    let root = PostgresCommitCoordinator::with_pool(pool.clone())
        .await
        .unwrap();
    let view = root
        .open_thread_projection(&ThreadId("thread-a".into()))
        .await
        .unwrap();
    assert!(
        view.commit(ThreadCommit::assemble(
            ThreadId("thread-b".into()),
            ended("run-b"),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .is_err(),
        "R1/E1"
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runtime_commit")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "R1/E2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "live PostgreSQL throughput observation, run separately from correctness suites"]
async fn independent_thread_commit_throughput_observation() {
    // Causes: C1 all commits target independent Threads; C2 client concurrency
    // is 1, 8, or 32; C3 each commit carries only a terminal Run disposition.
    // Effects: E1 every commit succeeds and allocates a unique dense sequence;
    // E2 print the actual application commit rate for each concurrency level.
    // Constraint: this is one local PostgreSQL primary without Worker/HTTP or
    // synchronous standby, so it diagnoses this store's lock path and cannot
    // establish fleet throughput. Decision rules: P1=C1+C2(1)+C3 -> E1+E2;
    // P2=C1+C2(8)+C3 -> E1+E2; P3=C1+C2(32)+C3 -> E1+E2.
    observe_commit_throughput("t_pg_commit_perf", CommitPayload::Minimal).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "live PostgreSQL throughput observation, run separately from correctness suites"]
async fn multi_fact_commit_throughput_observation() {
    // Causes: C1 each independent ThreadCommit carries eight ordered Messages,
    // eight StateCommands and eight Events; C2 concurrency is 1, 8 or 32.
    // Effects: E1 all facts remain atomically committed in order with dense
    // global sequences; E2 print application commit rate for comparison with
    // the same database, schema shape and client concurrency after bulk writes.
    // Rules M1/M2/M3 are C1+C2(1/8/32) -> E1+E2. This tests transaction
    // round trips and lock hold, not Worker/HTTP or a production Cell SLO.
    observe_commit_throughput("t_pg_commit_many_perf", CommitPayload::Many).await;
}

#[tokio::test]
async fn bulk_fact_chunks_replay_in_original_order_after_reconnect() {
    // Causes: C1 one ThreadCommit has 257 facts of each kind, crossing the
    // bounded SQL chunk; C2 a new coordinator reads the committed Thread prefix.
    // Effects: E1 every Message, StateCommand and supplied Event retains source
    // order alongside the Run's automatically staged lifecycle events;
    // E2 there is one dense commit sequence and one terminal Run. Rule B1=
    // C1+C2 -> E1+E2. This protects the portable row-id read order while the
    // Postgres adapter changes only how it submits rows within one transaction.
    let Some(pool) = schema_pool("t_pg_bulk_order").await else {
        return;
    };
    let thread = ThreadId("bulk-order-thread".into());
    let run = RunId("bulk-order-run".into());
    let messages: Vec<_> = (0..257)
        .map(|index| message(&format!("bulk-{index:03}"), "ordered"))
        .collect();
    let state: Vec<_> = (0..257)
        .map(|index| {
            StateCommand::set(
                Scope::Thread,
                MergePolicy::Disjoint,
                format!("bulk.state.{index:03}"),
                serde_json::json!(index),
            )
        })
        .collect();
    let events: Vec<_> = (0..257)
        .map(|index| Draft {
            kind: EventKind::RunStateChanged,
            payload: serde_json::json!({"index": index}),
        })
        .collect();
    let coordinator = PostgresCommitCoordinator::with_pool(pool.clone())
        .await
        .unwrap();
    let record = coordinator
        .commit(ThreadCommit::assemble(
            thread.clone(),
            RunDisposition::ended(run.clone(), EndCause::NaturalEnd),
            true,
            messages.clone(),
            state.clone(),
            events,
        ))
        .await
        .unwrap();
    assert_eq!(record.sequence, 1, "B1/E2");
    drop(coordinator);
    let reopened = PostgresCommitCoordinator::with_existing_pool(pool)
        .await
        .unwrap();
    let recovered = reopened.recovery_snapshot(&thread, &run).await.unwrap();
    assert_eq!(recovered.messages, messages, "B1/E1 messages");
    assert_eq!(recovered.state, state, "B1/E1 state");
    let supplied_events: Vec<_> = recovered
        .events
        .iter()
        .filter(|event| event.payload.get("index").is_some())
        .collect();
    assert_eq!(supplied_events.len(), 257, "B1/E1 supplied events");
    for (index, event) in supplied_events.iter().enumerate() {
        assert_eq!(event.payload["index"], serde_json::json!(index), "B1/E1");
    }
    assert_eq!(reopened.commit_count(), 1, "B1/E2");
}

#[tokio::test]
async fn later_bulk_chunk_failure_rolls_back_every_fact_and_reuses_sequence() {
    // Causes: C1 the second Message chunk violates a database constraint
    // after the first 256 rows were inserted; C2 the caller then retries one
    // valid commit. Effects: E1 no partial facts or run survive C1; E2 C2 gets
    // dense sequence one. Rules F1=C1 -> E1, F2=C1+C2 -> E2. A database error
    // must not turn chunking into a partial public commit.
    let Some(pool) = schema_pool("t_pg_bulk_rollback").await else {
        return;
    };
    let coordinator = PostgresCommitCoordinator::with_pool(pool.clone())
        .await
        .unwrap();
    sqlx::query(
        "ALTER TABLE runtime_message ADD CONSTRAINT reject_bulk_last \
         CHECK (data->>'id' <> 'bulk-fail')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let mut messages: Vec<_> = (0..256)
        .map(|index| message(&format!("bulk-{index:03}"), "before failure"))
        .collect();
    messages.push(message("bulk-fail", "rejected second chunk"));
    assert!(
        coordinator
            .commit(ThreadCommit::assemble(
                ThreadId("bulk-failed-thread".into()),
                ended("bulk-failed-run"),
                true,
                messages,
                Vec::new(),
                Vec::new(),
            ))
            .await
            .is_err(),
        "F1 constraint rejects second chunk"
    );
    for table in ["runtime_commit", "runtime_message", "runtime_run_record"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "F1/E1 {table}");
    }
    let retry = coordinator
        .commit(ThreadCommit::assemble(
            ThreadId("bulk-retry-thread".into()),
            ended("bulk-retry-run"),
            true,
            vec![message("bulk-good", "retry")],
            Vec::new(),
            Vec::new(),
        ))
        .await
        .unwrap();
    assert_eq!(retry.sequence, 1, "F2/E2");
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CommitPayload {
    Minimal,
    Many,
}

async fn observe_commit_throughput(schema: &'static str, payload: CommitPayload) {
    let Some(pool) = schema_pool(schema).await else {
        panic!("explicit performance run requires PostgreSQL");
    };
    let store = Arc::new(
        PostgresCommitCoordinator::with_pool(pool)
            .await
            .expect("commit coordinator"),
    );
    const COMMITS: usize = 128;
    for (phase, concurrency) in [1, 8, 32].into_iter().enumerate() {
        let started = std::time::Instant::now();
        let mut tasks = tokio::task::JoinSet::new();
        let mut sequences = Vec::with_capacity(COMMITS);
        for index in 0..COMMITS {
            let store = store.clone();
            tasks.spawn(async move {
                let run = RunId(format!("perf-run-{concurrency}-{index}"));
                let facts = if payload == CommitPayload::Many { 8 } else { 0 };
                let commit = ThreadCommit::assemble(
                    ThreadId(format!("perf-thread-{concurrency}-{index}")),
                    RunDisposition::ended(run, EndCause::NaturalEnd),
                    true,
                    (0..facts)
                        .map(|fact| {
                            message(
                                &format!("perf-message-{concurrency}-{index}-{fact}"),
                                "benchmark message",
                            )
                        })
                        .collect(),
                    (0..facts)
                        .map(|fact| {
                            StateCommand::set(
                                Scope::Thread,
                                MergePolicy::Disjoint,
                                format!("perf.state.{fact}"),
                                serde_json::json!(fact),
                            )
                        })
                        .collect(),
                    (0..facts)
                        .map(|fact| Draft {
                            kind: EventKind::RunStateChanged,
                            payload: serde_json::json!({"fact": fact}),
                        })
                        .collect(),
                );
                store.commit(commit).await.expect("independent commit")
            });
            if tasks.len() >= concurrency {
                sequences.push(tasks.join_next().await.unwrap().unwrap().sequence);
            }
        }
        while let Some(result) = tasks.join_next().await {
            sequences.push(result.unwrap().sequence);
        }
        sequences.sort_unstable();
        let first = (phase * COMMITS + 1) as u64;
        let last = ((phase + 1) * COMMITS) as u64;
        assert_eq!(sequences, (first..=last).collect::<Vec<_>>(), "P1-P3/E1");
        let elapsed = started.elapsed().as_secs_f64();
        println!(
            "commit_perf profile={} independent_threads={} concurrency={concurrency} seconds={elapsed:.3} commits_per_s={:.1}",
            if payload == CommitPayload::Many {
                "many"
            } else {
                "minimal"
            },
            COMMITS,
            COMMITS as f64 / elapsed
        );
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
async fn conformance_transcript_snapshots_freeze_append_only_prefix() {
    let Some(store) = conformance_store("t_c_transcript").await else {
        return;
    };
    awaken_store_conformance::transcript_snapshots_freeze_append_only_prefix(&store).await;
}

#[tokio::test]
async fn conformance_terminal_run_is_fenced() {
    let Some(store) = conformance_store("t_c_fence").await else {
        return;
    };
    awaken_store_conformance::terminal_run_is_fenced(&store).await;
}

#[tokio::test]
async fn conformance_resume_ticket_awaits_then_clears() {
    let Some(store) = conformance_store("t_c_wait").await else {
        return;
    };
    awaken_store_conformance::resume_ticket_awaits_then_clears(&store).await;
}

#[tokio::test]
async fn conformance_open_wait_selects_only_the_latest_run() {
    let Some(store) = conformance_store("t_c_open_wait_latest").await else {
        return;
    };
    awaken_store_conformance::open_wait_selects_only_the_latest_run(&store).await;
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
async fn conformance_recovery_snapshot_is_consistent() {
    let Some(store) = conformance_store("t_c_recovery").await else {
        return;
    };
    awaken_store_conformance::recovery_snapshot_is_consistent(&store).await;
}

#[tokio::test]
async fn conformance_commit_operation_is_idempotent_and_cas() {
    let Some(store) = conformance_store("t_c_operation").await else {
        return;
    };
    awaken_store_conformance::commit_operation_is_idempotent_and_cas(&store).await;
}

#[tokio::test]
async fn conformance_concurrent_operations_cas_one_winner() {
    let Some(store) = conformance_store("t_c_operation_race").await else {
        return;
    };
    awaken_store_conformance::concurrent_operations_cas_one_winner(&store).await;
}

#[tokio::test]
async fn conformance_empty_store_reads_are_absent() {
    let Some(store) = conformance_store("t_c_empty").await else {
        return;
    };
    awaken_store_conformance::empty_store_reads_are_absent(&store).await;
}

// Postgres projects committed state commands back through the `committed_state`
// read port (built from durable `runtime_state_command` rows on demand), so a resumed
// run replays its accumulated state from durable truth — it runs the shared
// `committed_state_replays` conformance case (as inmem/fs/sqlite do).
#[tokio::test]
async fn conformance_committed_state_replays() {
    let Some(store) = conformance_store("t_c_state").await else {
        return;
    };
    awaken_store_conformance::committed_state_replays(&store).await;
}

#[tokio::test]
async fn conformance_delegation_and_tool_state_commit_atomically() {
    let Some(store) = conformance_store("t_c_delegation_atomic").await else {
        return;
    };
    awaken_store_conformance::delegation_and_tool_state_commit_atomically(&store).await;
}

// Durable state replay across a reconnect: committed state commands are served back
// through a scoped execution view opened by a fresh coordinator, proving the
// projection derives from durable `runtime_state_command` rows.
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
                run: running("r-state"),
                messages: vec![],
                state: commands.clone(),
                events: vec![],
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

    // ...and a fresh authority reads them without loading unrelated history.
    let reopened = PostgresCommitCoordinator::with_pool(pool.clone())
        .await
        .expect("reconnect");
    let view = reopened
        .open_thread_projection(&thread)
        .await
        .expect("Thread view");
    assert_eq!(
        CommittedThreadView::committed_state(&view, &thread),
        commands,
        "committed state replays from durable truth after a reconnect"
    );
}

#[tokio::test]
async fn reconnect_reads_a_valid_persisted_optional_resume_ticket() {
    let Some(pool) = schema_pool("t_legacy_resume_reopen").await else {
        return;
    };
    let thread = ThreadId("thread-1".to_string());
    let run = RunId("run-legacy-resume".to_string());
    let store = PostgresCommitCoordinator::with_pool(pool.clone())
        .await
        .expect("coordinator");
    store
        .commit(ThreadCommit {
            thread_id: thread.clone(),
            run: RunDisposition::awaiting(ResumeTicket::new(
                "corr-1",
                run.clone(),
                thread.clone(),
                "snap-1",
                "fp-1",
                AwaitTarget::Pause(PauseReason::Manual),
            )),
            messages: vec![],
            state: vec![],
            events: vec![],
        })
        .await
        .expect("commit awaiting");

    // Cause/effect rule: a valid pre-closed-shape ticket is durable committed
    // truth. A fresh process must converge it to the one closed AwaitTarget;
    // malformed products remain rejected by the contract test. This exercises
    // the production scoped recovery path rather than adding a second decoder.
    let legacy = serde_json::json!({
        "correlation_id": "corr-1",
        "run_id": "run-legacy-resume",
        "thread_id": "thread-1",
        "snapshot_id": "snap-1",
        "catalog_fingerprint": "fp-1",
        "reason": "ExternalEvent",
        "call_id": "call-1",
        "pending_tool": {"tool_id": "tool-1", "arguments": {"cmd": "true"}},
        "deadline_ms": null
    });
    sqlx::query("UPDATE runtime_waiting SET ticket = $1 WHERE run_id = $2")
        .bind(Json(legacy))
        .bind(&run.0)
        .execute(&pool)
        .await
        .expect("replace ticket with historical wire shape");
    drop(store);

    let reopened = PostgresCommitCoordinator::with_pool(pool)
        .await
        .expect("reconnect");
    let view = reopened
        .open_thread_projection(&thread)
        .await
        .expect("Thread view");
    let ticket = CommittedThreadView::resume_ticket(&view, &run).expect("resume ticket");
    assert_eq!(
        ticket.target(),
        &AwaitTarget::ToolCall {
            reason: ToolAwaitReason::ClientExecution,
            call_id: "call-1".into(),
            tool: PendingTool {
                tool_id: "tool-1".into(),
                arguments: serde_json::json!({"cmd": "true"}),
            },
            authority: awaken_agent_contract::agent::awaiting::ToolCallAuthority::LegacyUnspecified,
        }
    );
}

#[tokio::test]
async fn corrupt_waiting_ticket_is_isolated_from_postgres_restart_and_recovery() {
    // Cause/effect graph: C1=Awaiting ThreadCommit stores complete Run facts;
    // C2=only its JSONB waiting ticket becomes semantically invalid; C3=a cold
    // coordinator opens, reads authoritative open-wait, and takes a recovery
    // snapshot. Effects: E1=startup succeeds; E2=Run/transcript/state/audit and
    // fences survive; E3=no reply authority is exposed; E4=the corrupt row is
    // retained for canonical Runtime interruption to consume atomically.
    // Decision table: PG1 valid facts+valid ticket=>ordinary recovery; PG2 valid
    // facts+corrupt ticket+C3=>E1-E4; PG3 corrupt non-ticket fact=>scoped
    // recovery fails closed. Constraint: this adapter never deletes waiting rows.
    let Some(pool) = schema_pool("t_corrupt_waiting_recovery").await else {
        return;
    };
    let thread = ThreadId("corrupt-waiting-thread".into());
    let run = RunId("corrupt-waiting-run".into());
    let store = PostgresCommitCoordinator::with_pool(pool.clone())
        .await
        .expect("coordinator");
    store
        .commit(ThreadCommit::assemble(
            thread.clone(),
            RunDisposition::awaiting(ResumeTicket::new(
                "corrupt-waiting-correlation",
                run.clone(),
                thread.clone(),
                "corrupt-waiting-snapshot",
                "corrupt-waiting-catalog",
                AwaitTarget::Pause(PauseReason::Manual),
            )),
            true,
            vec![message("corrupt-waiting-message", "retained")],
            vec![StateCommand::set(
                Scope::Run,
                MergePolicy::Disjoint,
                "test.corrupt-waiting.fact",
                serde_json::json!({"retained": true}),
            )],
            Vec::new(),
        ))
        .await
        .expect("PG2 complete Awaiting prefix");
    sqlx::query("UPDATE runtime_waiting SET ticket = $1 WHERE run_id = $2")
        .bind(Json(serde_json::json!({"not": "a ResumeTicket"})))
        .bind(&run.0)
        .execute(&pool)
        .await
        .expect("PG2 isolate waiting-ticket corruption");
    drop(store);

    let reopened = PostgresCommitCoordinator::with_pool(pool.clone())
        .await
        .expect("PG2/E1 cold hydrate");
    assert!(
        CommittedThreadView::resume_ticket(&reopened, &run).is_none(),
        "PG2/E3 compatibility projection"
    );
    assert!(
        reopened
            .authoritative_open_wait_for_thread(&thread)
            .await
            .expect("PG2 authoritative read stays available")
            .is_none(),
        "PG2/E3 authoritative projection"
    );
    let snapshot = reopened
        .recovery_snapshot(&thread, &run)
        .await
        .expect("PG2/E2 recovery prefix");
    assert_eq!(snapshot.runs[0].state, RunState::Awaiting, "PG2/E2");
    assert_eq!(snapshot.messages.len(), 1, "PG2/E2 transcript");
    assert_eq!(snapshot.state.len(), 1, "PG2/E2 state");
    assert!(
        snapshot
            .events
            .iter()
            .any(|event| event.kind == EventKind::RunStateChanged),
        "PG2/E2 audit"
    );
    assert!(snapshot.resume_tickets.is_empty(), "PG2/E3");
    assert_eq!(snapshot.thread_version, 1, "PG2/E2 fence");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM runtime_waiting WHERE run_id = $1")
            .bind(&run.0)
            .fetch_one(&pool)
            .await
            .expect("PG2/E4 retained row"),
        1,
        "PG2/E4"
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

    let record = coordinator.commit(commit).await.expect("commit");
    assert_eq!(record.sequence, 1);
    assert_eq!(coordinator.commit_count(), 1);

    let run =
        CommittedThreadView::run(&coordinator, &RunId("run-1".to_string())).expect("run record");
    assert_eq!(run.state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(run.thread_id, thread);

    let messages = CommittedThreadView::committed_messages(&coordinator, &thread);
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
async fn rolled_back_append_reuses_the_dense_global_successor() {
    // Cause/effect decision table: C1 empty history + forced event conflict
    // after the commit-row insert -> E1 whole transaction rolls back; C2 remove
    // the fault and retry -> E2 sequence 1, not 2; C3 committed successor -> E3
    // exactly one commit row. This exercises the production allocator and its
    // later-write rollback boundary against a real database when available.
    let Some(pool) = schema_pool("t_dense_rollback").await else {
        return;
    };
    let coordinator = PostgresCommitCoordinator::with_pool(pool.clone())
        .await
        .expect("coordinator");
    sqlx::query(
        "INSERT INTO runtime_event (sequence, run_id, kind, payload) VALUES ($1, $2, $3, $4)",
    )
    .bind(1_000_i64)
    .bind("fault")
    .bind(Json(&EventKind::RunStateChanged))
    .bind(Json(&serde_json::json!({"fault": true})))
    .execute(&pool)
    .await
    .expect("install later-write conflict");
    let commit = ThreadCommit {
        thread_id: ThreadId("thread-dense".into()),
        run: running("run-dense"),
        messages: Vec::new(),
        state: Vec::new(),
        events: vec![Draft {
            kind: EventKind::RunStateChanged,
            payload: serde_json::json!({"attempt": 1}),
        }],
    };
    assert!(coordinator.commit(commit.clone()).await.is_err(), "C1/E1");
    sqlx::query("DELETE FROM runtime_event WHERE run_id = 'fault'")
        .execute(&pool)
        .await
        .expect("remove fault");
    assert_eq!(
        coordinator.commit(commit).await.unwrap().sequence,
        1,
        "C2/E2"
    );
    let sequences: Vec<i64> =
        sqlx::query_scalar("SELECT sequence FROM runtime_commit ORDER BY sequence")
            .fetch_all(&pool)
            .await
            .expect("read dense history");
    assert_eq!(sequences, vec![1], "C3/E3");
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
                run: ended(&format!("run-{expected}")),
                messages: vec![],
                state: vec![],
                events: vec![],
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

    let commit = |fact: RunDisposition, msg_id: &str, text: &str| ThreadCommit {
        thread_id: thread.clone(),
        run: fact,
        messages: vec![message(msg_id, text)],
        state: vec![],
        events: vec![],
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
        CommittedThreadView::run(&coordinator, &run).map(|record| record.state),
        Some(RunState::Ended(EndCause::NaturalEnd)),
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
async fn resume_ticket_awaits_then_clears() {
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
            run: awaiting_disposition("run-1"),
            messages: vec![],
            state: vec![],
            events: vec![],
        })
        .await
        .expect("await");
    assert!(CommittedThreadView::resume_ticket(&coordinator, &run).is_some());

    coordinator
        .commit(ThreadCommit {
            thread_id: ThreadId("thread-1".to_string()),
            run: ended("run-1"),
            messages: vec![],
            state: vec![],
            events: vec![],
        })
        .await
        .expect("resume to terminal");
    assert!(
        CommittedThreadView::resume_ticket(&coordinator, &run).is_none(),
        "a terminal run clears its ticket (fail closed)"
    );
}

#[tokio::test]
async fn authoritative_latest_and_wait_track_cross_replica_runs() {
    /*
     * Awaiting-read cause/effect decision table.
     * Causes: C1 this coordinator's compatibility projection contains an older
     * Awaiting Run; C2 a peer commits a newer terminal Run on the same Thread;
     * C3 that peer then commits a still newer Awaiting Run; C4 a cold observer
     * opened before every commit and retains an empty compatibility projection.
     * Effects: E1 the
     * authoritative read returns no ticket after C2 instead of blocking on C1;
     * E2 after C3 it returns exactly the newest durable ticket even though the
     * observer projection never advanced; E3 the authoritative latest-Run read
     * returns the peer's exact Run/state while C4 still reports no projected
     * latest Run. Rules: W1=C1+C2=>E1; W2=C1+C2+C3=>E2;
     * W3=C2+C4=>E3. This is the cross-protocol recovery race: admission,
     * settlement, and resume must follow shared PostgreSQL truth, not a
     * process-local cache.
     */
    let Some(pool) = schema_pool("t_authoritative_wait").await else {
        return;
    };
    let observer = PostgresCommitCoordinator::with_pool(pool.clone())
        .await
        .expect("observer");
    let peer = PostgresCommitCoordinator::with_existing_pool(pool.clone())
        .await
        .expect("peer");
    let cold = PostgresCommitCoordinator::with_existing_pool(pool)
        .await
        .expect("cold observer");
    let thread = ThreadId("thread-1".to_string());

    observer
        .commit(ThreadCommit {
            thread_id: thread.clone(),
            run: awaiting_disposition("run-old"),
            messages: vec![],
            state: vec![],
            events: vec![],
        })
        .await
        .expect("old wait");
    peer.commit(ThreadCommit {
        thread_id: thread.clone(),
        run: ended("run-new"),
        messages: vec![],
        state: vec![],
        events: vec![],
    })
    .await
    .expect("new terminal");

    assert_eq!(
        CommittedThreadView::latest_run(&cold, &thread),
        None,
        "W3/C4 cold compatibility projection remains empty"
    );
    assert_eq!(
        cold.authoritative_latest_run_record(&thread)
            .await
            .expect("W3 authoritative latest read"),
        Some(awaken_agent_contract::agent::run::Record {
            id: RunId("run-new".into()),
            thread_id: thread.clone(),
            state: RunState::Ended(EndCause::NaturalEnd),
        }),
        "W3/E3 shared committed latest Run bypasses the cold projection"
    );

    assert!(
        observer
            .authoritative_open_wait_for_thread(&thread)
            .await
            .expect("authoritative terminal read")
            .is_none(),
        "a peer's newer terminal Run supersedes the observer's stale wait"
    );

    peer.commit(ThreadCommit {
        thread_id: thread.clone(),
        run: RunDisposition::awaiting(ticket("run-latest", "thread-1")),
        messages: vec![],
        state: vec![],
        events: vec![],
    })
    .await
    .expect("latest wait");
    let (run_id, latest_ticket) = observer
        .authoritative_open_wait_for_thread(&thread)
        .await
        .expect("authoritative awaiting read")
        .expect("latest wait exists");
    assert_eq!(run_id.0, "run-latest");
    assert_eq!(latest_ticket.run_id.0, "run-latest");
}

#[tokio::test]
async fn authoritative_state_tracks_cross_replica_commits() {
    /*
     * State-read cause/effect decision table.
     * Causes: C1 an observer opens before a peer commit; C2 the peer commits
     * one Thread-scoped cumulative-usage state command; C3 no local commit ever
     * advances the observer projection. Effects: E1 the compatibility view
     * remains empty, proving the stale-replica precondition; E2 the authoritative
     * read returns the exact durable command once and in commit order. Rule
     * S1=C1+C2+C3=>E1+E2. This covers budget admission after Coordinator restart
     * without introducing a second usage ledger or synchronizing peer caches.
     */
    let Some(pool) = schema_pool("t_authoritative_state").await else {
        return;
    };
    let observer = PostgresCommitCoordinator::with_pool(pool.clone())
        .await
        .expect("observer");
    let peer = PostgresCommitCoordinator::with_existing_pool(pool)
        .await
        .expect("peer");
    let thread = ThreadId("thread-state-peer".into());
    let command = StateCommand::set(
        Scope::Thread,
        MergePolicy::Commutative,
        "__usage",
        serde_json::json!({"by_model":{"model-a":{"prompt_tokens":2,"completion_tokens":2}}}),
    );
    peer.commit(ThreadCommit {
        thread_id: thread.clone(),
        run: ended("run-state-peer"),
        messages: vec![],
        state: vec![command.clone()],
        events: vec![],
    })
    .await
    .expect("peer state commit");

    assert!(
        CommittedThreadView::committed_state(&observer, &thread).is_empty(),
        "S1/E1 observer compatibility projection remains stale"
    );
    assert_eq!(
        observer
            .authoritative_committed_state(&thread)
            .await
            .expect("S1 authoritative state read"),
        vec![command],
        "S1/E2 shared committed state bypasses the observer projection"
    );
}

#[tokio::test]
async fn migration_phase_applies_schema_then_runtime_verifies_and_commits() {
    // Cause/effect decision table for schema access:
    // R1 empty schema + migrate -> portable and PG-only bundles are applied
    // without hydrating a runtime projection.
    // R2 R1 ledger + connect_existing -> both bundles are verified, bounded
    // startup succeeds, and commits remain writable without startup DDL.
    // R3 either bundle missing/drifted -> connect_existing fails closed (the
    // shared scoped-migration verify suite owns those ledger failure cases).
    let schema = "t_connect";
    if schema_pool(schema).await.is_none() {
        return;
    }

    let url = database_url_in_schema(schema);
    PostgresCommitCoordinator::migrate(&url, 10)
        .await
        .expect("migrate schema only");
    let coordinator = PostgresCommitCoordinator::connect_existing(&url, 10)
        .await
        .expect("verify and connect existing");
    coordinator
        .commit(ThreadCommit {
            thread_id: ThreadId("thread-1".to_string()),
            run: ended("run-1"),
            messages: vec![],
            state: vec![],
            events: vec![],
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
            run: ended("run-1"),
            messages: vec![],
            state: vec![],
            events: vec![],
        })
        .await
        .expect_err("insert fails");
    assert!(matches!(
        err,
        awaken_agent_contract::thread::commit::coordinator::Error::Rejected(_)
    ));
}

#[tokio::test]
async fn thread_projection_opens_from_postgres_after_reconnect() {
    // Test design — restart state-machine contract:
    // C1 a committed Message/Run/Wait survives restart; C2 one Thread view is
    // opened on demand. E1 startup reads only the fence; E2 the scoped view
    // contains the complete atomic prefix. R1=C1+C2 -> E1+E2.
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
                run: awaiting_disposition("run-1"),
                messages: vec![message("m1", "persisted")],
                state: vec![],
                events: vec![],
            })
            .await
            .expect("commit");
    } // coordinator A dropped — simulate a restart

    // A fresh coordinator opens without history, then loads just one Thread.
    let restarted = PostgresCommitCoordinator::with_pool(pool.clone())
        .await
        .expect("coordinator b");
    assert_eq!(restarted.commit_count(), 1, "fence survives restart");
    let thread = ThreadId("thread-1".to_string());
    let view = restarted
        .open_thread_projection(&thread)
        .await
        .expect("Thread view");
    assert_eq!(
        CommittedThreadView::committed_messages(&view, &thread)[0]
            .id
            .0,
        "m1"
    );
    assert!(
        CommittedThreadView::run(&view, &RunId("run-1".to_string())).is_some(),
        "run record rehydrated"
    );
    assert!(
        CommittedThreadView::resume_ticket(&view, &RunId("run-1".to_string())).is_some(),
        "active ticket rehydrated"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_thread_open_observes_one_complete_postgres_snapshot() {
    // Test design — repeated concurrency history test for snapshot isolation:
    // race one atomic commit against Thread open. The only legal observations are
    // the complete prefix before the commit or the complete prefix after it.
    // A mixed result (old sequence with new message/run, or new sequence with a
    // missing fact) represents no serial database state and fails immediately.
    let Some(pool) = schema_pool("t_hydrate_snapshot").await else {
        return;
    };
    let writer = PostgresCommitCoordinator::with_pool(pool.clone())
        .await
        .expect("writer");

    for ordinal in 0..64_u64 {
        let before = writer.commit_count();
        let thread = ThreadId(format!("snapshot-thread-{ordinal}"));
        let run = RunId(format!("snapshot-run-{ordinal}"));
        let commit = ThreadCommit {
            thread_id: thread.clone(),
            run: running(&run.0),
            messages: vec![message(&format!("snapshot-message-{ordinal}"), "snapshot")],
            state: vec![],
            events: vec![],
        };
        let (committed, hydrated) = tokio::join!(
            writer.commit(commit),
            writer.open_thread_projection(&thread),
        );
        committed.expect("writer commit");
        let hydrated = hydrated.expect("open concurrent snapshot");
        let observed = hydrated.commit_count();
        let has_message = !CommittedThreadView::committed_messages(&hydrated, &thread).is_empty();
        let has_run = CommittedThreadView::run(&hydrated, &run).is_some();

        match observed {
            value if value == before => {
                assert!(!has_message, "old snapshot cannot include the new message");
                assert!(!has_run, "old snapshot cannot include the new run");
            }
            value if value == before + 1 => {
                assert!(has_message, "new snapshot includes every committed message");
                assert!(has_run, "new snapshot includes the committed run");
            }
            other => panic!(
                "hydrate observed non-serial sequence {other}, expected {before} or {}",
                before + 1
            ),
        }
    }
}

#[tokio::test]
async fn operation_receipt_survives_reconnect() {
    let Some(pool) = schema_pool("t_operation_receipt_reconnect").await else {
        return;
    };
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
        let coordinator = PostgresCommitCoordinator::with_pool(pool.clone())
            .await
            .expect("coordinator a");
        assert!(
            !coordinator
                .commit_operation(operation.clone())
                .await
                .unwrap()
                .duplicate
        );
    }
    let reopened = PostgresCommitCoordinator::with_pool(pool)
        .await
        .expect("coordinator b");
    assert!(
        reopened
            .commit_operation(operation)
            .await
            .expect("durable duplicate receipt")
            .duplicate
    );
    assert_eq!(
        reopened
            .authoritative_committed_messages(&ThreadId("receipt-thread".into()))
            .await
            .expect("committed transcript")
            .len(),
        1
    );
}

#[tokio::test]
async fn peer_lifecycle_feed_reads_authoritative_postgres_without_projection_refresh() {
    // Cause/effect graph: C1 peer starts before writer commits; C2 writer commits
    // lifecycle commits 1..=5 and one message; C3 commit 3 is a reclaimed
    // Awaiting attempt; C4 the feed pages at two and replays the last cursor.
    // Effects:
    // E1 compatibility projection remains stale; E2 authoritative reads see
    // committed truth; E3 kinds classify
    // Running/Awaiting/Rescheduled/Resumed/Completed; E4
    // encoded/source cursor pairs are [(1000,1),(2000,2)] then
    // [(3000,3),(4000,4),(5000,5)]; E5 the exclusive last cursor is idempotent.
    //
    // | Rule | C1 | C2 | C3 | C4 | Effects |
    // |---|---|---|---|---|---|
    // | P1 | T | F | - | - | E1 |
    // | P2 | T | T | T | F | E1,E2,E3,E4 |
    // | P3 | T | T | T | T | E1,E2,E3,E4 |
    // | P4 | T | T | T | last | E5 |
    // Constraint/Invariant: PostgreSQL committed rows, not a process-local
    // projection, own lifecycle feed truth. Coordinator construction completes
    // the mandatory v10 migration first, so this query consumes only canonical
    // RunStateChanged; historical spelling normalization belongs exclusively to
    // that migration. Decision rule: P1-P4 cover pre-write staleness,
    // authoritative paging, replay, and exclusive last-cursor behavior after
    // canonicalization.
    let Some(pool) = schema_pool("t_active_active_lifecycle").await else {
        return;
    };
    let writer = PostgresCommitCoordinator::with_pool(pool.clone())
        .await
        .expect("writer coordinator");
    let peer = PostgresCommitCoordinator::with_pool(pool)
        .await
        .expect("peer coordinator before commits");
    let thread = ThreadId("active-active-thread".into());
    let run = RunId("active-active-run".into());

    for (index, disposition) in [
        RunDisposition::running(run.clone()),
        RunDisposition::awaiting(ticket(&run.0, &thread.0)),
        RunDisposition::running(run.clone()),
        RunDisposition::ended(run.clone(), EndCause::NaturalEnd),
    ]
    .into_iter()
    .enumerate()
    {
        writer
            .commit(ThreadCommit::assemble(
                thread.clone(),
                disposition,
                true,
                if index == 0 {
                    vec![message("active-active-message", "peer-visible")]
                } else {
                    Vec::new()
                },
                Vec::new(),
                Vec::new(),
            ))
            .await
            .expect("commit lifecycle transition");
        if index == 1 {
            writer
                .commit(ThreadCommit::rescheduled(
                    thread.clone(),
                    RunDisposition::awaiting(ticket(&run.0, &thread.0)),
                    2,
                ))
                .await
                .expect("commit recovered lifecycle receipt");
        }
    }

    assert!(
        CommittedThreadView::run(&peer, &run).is_none(),
        "P1/P3 the peer's synchronous compatibility projection remains stale"
    );
    assert_eq!(
        peer.authoritative_run_record(&run)
            .await
            .expect("P2 authoritative exact read")
            .expect("P2 committed Run exists")
            .state,
        RunState::Ended(EndCause::NaturalEnd),
        "P2 exact active-active reconciliation reads committed truth"
    );
    assert!(
        CommittedThreadView::committed_messages(&peer, &thread).is_empty(),
        "P1/P3 the synchronous peer transcript remains stale"
    );
    assert_eq!(
        peer.authoritative_committed_messages(&thread)
            .await
            .expect("P2 authoritative transcript"),
        vec![message("active-active-message", "peer-visible")],
        "P2 public projection reads every peer-committed message"
    );
    let first = peer
        .events_after(RunLifecycleCursor::default(), 2)
        .await
        .expect("authoritative first page");
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
        "P3/E4"
    );
    let second = peer
        .events_after(first.next_cursor, 8)
        .await
        .expect("authoritative second page");
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
        "P3/E4"
    );
    assert!(
        second
            .events
            .iter()
            .all(|event| event.thread_id == thread && event.run_id == run),
        "the authoritative feed retains Run and Thread ownership"
    );
    let replay = peer
        .events_after(second.next_cursor, 8)
        .await
        .expect("idempotent exclusive replay");
    assert!(replay.events.is_empty(), "P4/E5");
    assert_eq!(replay.next_cursor, second.next_cursor, "P4/E5");
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
                    run: ended(&format!("run-{i}")),
                    messages: vec![message(&format!("m{i}"), "x")],
                    state: Vec::new(),
                    events: Vec::new(),
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

#[tokio::test]
async fn recovery_time_truncates_postgres_submilliseconds_and_normalizes_timezone() {
    // D8 SQL's microsecond clock x offset spelling -> floor to epoch millis,
    // including the carry boundary. Read via the actual fenced recovery port;
    // no formatter or mocked database acts as the conversion oracle.
    let Some(pool) = schema_pool("commit_time_precision").await else {
        return;
    };
    let store = PostgresCommitCoordinator::with_pool(pool.clone())
        .await
        .unwrap();
    let thread = ThreadId("precision".into());
    let run = RunId("precision".into());
    store
        .commit(ThreadCommit {
            thread_id: thread.clone(),
            run: running("precision"),
            messages: vec![],
            state: vec![],
            events: vec![],
        })
        .await
        .unwrap();
    for (fraction, expected) in [(0, 1_000), (1_999, 1_001), (999_999, 1_999)] {
        for (hour, offset) in [("00", "Z"), ("08", "+08:00")] {
            let observation = format!("1970-01-01T{hour}:00:01.{fraction:06}{offset}");
            sqlx::query("UPDATE runtime_commit SET committed_at = $1::text::timestamptz")
                .bind(&observation)
                .execute(&pool)
                .await
                .unwrap();
            let snapshot = store.recovery_snapshot(&thread, &run).await.unwrap();
            assert_eq!(
                snapshot.commit_timestamps.get(&1),
                Some(&expected),
                "D8 {observation}"
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
    let Some(pool) = schema_pool("t_recovery_log_owned_scope").await else {
        return;
    };
    let store = PostgresCommitCoordinator::with_pool(pool.clone())
        .await
        .unwrap();
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
                awaken_agent_contract::thread::commit::RunDisposition::awaiting(ticket),
                true,
                vec![Message::text(
                    MessageId(format!("scope-message-{i}")),
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
    for mutation in [
        "UPDATE runtime_run_record SET thread_id=CASE WHEN thread_id='scope-thread-0' THEN 'scope-thread-1' ELSE 'scope-thread-0' END",
        "DELETE FROM runtime_run_record",
    ] {
        sqlx::query(mutation).execute(&pool).await.unwrap();
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
}
