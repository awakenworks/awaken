//! Live Postgres tests for the durable commit boundary.
//!
//! They run against a real Postgres (the commit transaction and projection
//! hydration cannot be exercised otherwise). The URL comes from
//! `AWAKEN_TEST_DATABASE_URL`, defaulting to the local dev container. If no
//! Postgres is reachable the test prints a skip notice and returns, so the suite
//! still passes on a machine without a database.

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::state::{Command as StateCommand, MergePolicy, Scope};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::agent::waiting::{WaitingReason, WaitingTicket};
use awaken_agent_contract::commit::coordinator::Coordinator;
use awaken_agent_contract::commit::staged::ThreadCommit;
use awaken_agent_contract::event::draft::Draft;
use awaken_agent_contract::event::kind::Kind as EventKind;
use awaken_agent_contract::fact::run::Fact as RunFact;
use awaken_agent_contract::store::run_store::RunStore;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_store_postgres::PostgresCommitCoordinator;
use sqlx::Row;
use sqlx::postgres::PgPool;

fn database_url() -> String {
    std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
    })
}

/// Connect, or return `None` (with a skip notice) when no Postgres is reachable.
async fn pool() -> Option<PgPool> {
    match PgPool::connect(&database_url()).await {
        Ok(pool) => Some(pool),
        Err(err) => {
            println!("[skip] no Postgres reachable: {err}");
            None
        }
    }
}

/// Drop the bundle tables and the migration ledger for a prefix so each test
/// starts from a clean schema (isolated per `prefix`).
async fn reset(pool: &PgPool, prefix: &str) {
    for table in [
        "commit",
        "message",
        "state_command",
        "event",
        "run_record",
        "waiting",
        "schema_migrations",
    ] {
        let _ = sqlx::query(&format!("DROP TABLE IF EXISTS {prefix}_{table} CASCADE"))
            .execute(pool)
            .await;
    }
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

#[tokio::test]
async fn commit_persists_facts_messages_and_serves_reads() {
    let Some(pool) = pool().await else { return };
    let prefix = "t_commit";
    reset(&pool, prefix).await;

    let coordinator = PostgresCommitCoordinator::with_pool(pool.clone(), prefix)
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
            kind: EventKind::MessageCommitted,
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
    let states: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {prefix}_state_command"))
        .fetch_one(&pool)
        .await
        .expect("count states");
    assert_eq!(states, 1);
    let event_seq: i64 = sqlx::query(&format!("SELECT sequence FROM {prefix}_event"))
        .fetch_one(&pool)
        .await
        .expect("event row")
        .get("sequence");
    assert_eq!(event_seq, 1_000); // first commit (1) * 1000 + offset 0

    reset(&pool, prefix).await;
}

#[tokio::test]
async fn fence_increments_monotonically() {
    let Some(pool) = pool().await else { return };
    let prefix = "t_fence";
    reset(&pool, prefix).await;
    let coordinator = PostgresCommitCoordinator::with_pool(pool.clone(), prefix)
        .await
        .expect("coordinator");

    for expected in 1..=3u64 {
        let record = coordinator
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
        assert_eq!(record.sequence, expected);
    }
    assert_eq!(coordinator.commit_count(), 3);
    reset(&pool, prefix).await;
}

#[tokio::test]
async fn waiting_ticket_parks_then_clears() {
    let Some(pool) = pool().await else { return };
    let prefix = "t_waiting";
    reset(&pool, prefix).await;
    let coordinator = PostgresCommitCoordinator::with_pool(pool.clone(), prefix)
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
    reset(&pool, prefix).await;
}

#[tokio::test]
async fn connect_applies_migrations_and_serves_a_commit() {
    let Some(probe) = pool().await else { return };
    let prefix = "t_connect";
    reset(&probe, prefix).await;

    let coordinator = PostgresCommitCoordinator::connect(&database_url(), prefix)
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
    reset(&probe, prefix).await;
}

#[tokio::test]
async fn commit_maps_a_storage_failure_to_a_rejection() {
    let Some(pool) = pool().await else { return };
    let prefix = "t_fail";
    reset(&pool, prefix).await;
    let coordinator = PostgresCommitCoordinator::with_pool(pool.clone(), prefix)
        .await
        .expect("coordinator");

    // Remove a table the commit must write to, forcing the transaction to fail.
    sqlx::query(&format!("DROP TABLE {prefix}_commit"))
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
        awaken_agent_contract::commit::coordinator::Error::Rejected(_)
    ));
    reset(&pool, prefix).await;
}

#[tokio::test]
async fn projection_rehydrates_from_postgres_after_reconnect() {
    let Some(pool) = pool().await else { return };
    let prefix = "t_hydrate";
    reset(&pool, prefix).await;

    {
        let coordinator = PostgresCommitCoordinator::with_pool(pool.clone(), prefix)
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
    let restarted = PostgresCommitCoordinator::with_pool(pool.clone(), prefix)
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
    reset(&pool, prefix).await;
}
