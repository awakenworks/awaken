//! Full durable-ingress end to end on Postgres: the dispatch queue and the
//! commit boundary both persist, sharing one database (distinct scoped bundles).
//!
//! Proves the crown-jewel loop — a durable submit parks, the dispatch row
//! survives a restart, and a delivered decision wakes and resumes the run to a
//! committed terminal phase — against real storage. Skips when no Postgres is
//! reachable.

mod harness;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::store::run_store::RunStore;
use awaken_run_ingress::{
    DurableRunIngress, PendingInbox, PendingInput, PostgresDispatchStore, RunDispatch,
    RunExecutionRequest,
};
use awaken_runtime::RunIngress;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_store_postgres::PostgresCommitCoordinator;

use harness::{THREAD, TICKET, activation, database_url, pool, reset, tool_runtime};

fn pending(message_id: &str, correlation: &str, allow: bool) -> PendingInput {
    PendingInput {
        message_id: message_id.to_string(),
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId(THREAD.to_string()),
        correlation_id: correlation.to_string(),
        available_at_ms: None,
        result: ResumeResult::Decision { allow, note: None },
    }
}

#[tokio::test]
async fn durable_submit_parks_then_delivered_decision_resumes_on_postgres() {
    let Some(pool) = pool().await else { return };
    let prefix = "t_e2e";
    reset(&pool, prefix).await;

    let (runtime, ran) = tool_runtime();
    let commit = Arc::new(
        PostgresCommitCoordinator::with_pool(pool.clone(), prefix)
            .await
            .expect("commit"),
    );
    let store = Arc::new(
        PostgresDispatchStore::with_pool(pool.clone(), prefix)
            .await
            .expect("dispatch"),
    );
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());

    // Durable submit parks on the gate; the tool has not run.
    let phase = ingress
        .submit_background(activation("run-1"))
        .await
        .expect("submit");
    assert_eq!(phase, Phase::Waiting);
    assert_eq!(ran.load(Ordering::SeqCst), 0);

    // A delivered allow decision wakes and resumes the run to completion.
    let resumed = ingress
        .deliver_resume(
            PendingInput {
                message_id: "msg-1".to_string(),
                run_id: RunId("run-1".to_string()),
                thread_id: ThreadId(THREAD.to_string()),
                correlation_id: TICKET.to_string(),
                available_at_ms: None,
                result: ResumeResult::Decision {
                    allow: true,
                    note: None,
                },
            },
            0,
        )
        .await
        .expect("resume");
    assert_eq!(resumed, Phase::Ended(EndCause::NaturalEnd));
    assert_eq!(ran.load(Ordering::SeqCst), 1);

    // Committed truth is terminal.
    let record = RunStore::get(&*commit, &RunId("run-1".to_string())).expect("run record");
    assert_eq!(record.phase, Phase::Ended(EndCause::NaturalEnd));

    reset(&pool, prefix).await;
}

#[tokio::test]
async fn enqueued_dispatch_survives_a_restart() {
    let Some(pool) = pool().await else { return };
    let prefix = "t_durable";
    reset(&pool, prefix).await;

    // Enqueue a run, then drop the store to simulate a process restart.
    {
        let store = PostgresDispatchStore::with_pool(pool.clone(), prefix)
            .await
            .expect("dispatch a");
        store
            .enqueue(RunExecutionRequest::new(activation("run-1")))
            .await
            .expect("enqueue");
    }

    // A fresh store on the same database still has the accepted run to claim.
    let restarted = PostgresDispatchStore::with_pool(pool.clone(), prefix)
        .await
        .expect("dispatch b");
    let claimed = restarted
        .claim("worker", 1_000, 0)
        .await
        .expect("claim")
        .expect("the enqueued run survived restart");
    assert_eq!(claimed.request.run_id().0, "run-1");

    reset(&pool, prefix).await;
}

#[tokio::test]
async fn postgres_connect_applies_migrations_and_claim_recovers_a_lease() {
    let Some(probe) = pool().await else { return };
    let prefix = "t_pg_recover";
    reset(&probe, prefix).await;

    // connect() (not with_pool) applies the dispatch migrations on a fresh pool.
    let store = PostgresDispatchStore::connect(&database_url(), prefix)
        .await
        .expect("connect");
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .expect("enqueue");
    // Re-enqueue is idempotent.
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .expect("re-enqueue");

    // Claim with a zero lease, then a later claim reclaims the expired lease.
    assert!(store.claim("a", 0, 100).await.unwrap().is_some());
    let recovered = store.claim("b", 1_000, 101).await.unwrap();
    assert_eq!(
        recovered.map(|c| c.lease.owner),
        Some("b".to_string()),
        "the expired lease was reclaimed"
    );

    reset(&probe, prefix).await;
}

#[tokio::test]
async fn postgres_append_is_idempotent_and_stale_input_is_dropped() {
    let Some(pool) = pool().await else { return };
    let prefix = "t_pg_stale";
    reset(&pool, prefix).await;

    let (runtime, ran) = tool_runtime();
    let commit = Arc::new(
        PostgresCommitCoordinator::with_pool(pool.clone(), prefix)
            .await
            .expect("commit"),
    );
    let store = Arc::new(
        PostgresDispatchStore::with_pool(pool.clone(), prefix)
            .await
            .expect("dispatch"),
    );
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());

    assert_eq!(
        ingress
            .submit_background(activation("run-1"))
            .await
            .unwrap(),
        Phase::Waiting
    );

    // A duplicate append is a no-op on Postgres too (stale correlation, so it
    // does not resume the run).
    assert!(
        store
            .append(pending("dup", "old-ticket", true))
            .await
            .unwrap()
    );
    assert!(
        !store
            .append(pending("dup", "old-ticket", true))
            .await
            .unwrap()
    );

    // Stale input (wrong correlation) is dropped without resuming the run.
    let phase = ingress
        .deliver_resume(pending("stale", "old-ticket", true), 0)
        .await
        .expect("stale delivery");
    assert_eq!(phase, Phase::Waiting);
    assert_eq!(ran.load(Ordering::SeqCst), 0, "stale input did not resume");

    // The correctly-correlated input (the earlier "dup") now resumes the run.
    let phase = ingress
        .deliver_resume(pending("good", TICKET, true), 0)
        .await
        .expect("good delivery");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    assert_eq!(ran.load(Ordering::SeqCst), 1);
    let record = RunStore::get(&*commit, &RunId("run-1".to_string())).expect("record");
    assert_eq!(record.phase, Phase::Ended(EndCause::NaturalEnd));

    reset(&pool, prefix).await;
}

#[tokio::test]
async fn pending_revision_cas_on_postgres() {
    let Some(pool) = pool().await else { return };
    let prefix = "t_pg_cas";
    reset(&pool, prefix).await;
    let store = PostgresDispatchStore::with_pool(pool.clone(), prefix)
        .await
        .expect("dispatch");
    harness::assert_pending_revision_cas(&store).await;
    reset(&pool, prefix).await;
}

#[tokio::test]
async fn cross_thread_outbox_on_postgres() {
    let Some(pool) = pool().await else { return };
    let prefix = "t_pg_outbox";
    reset(&pool, prefix).await;
    let store = PostgresDispatchStore::with_pool(pool.clone(), prefix)
        .await
        .expect("dispatch");
    harness::assert_cross_thread_outbox(&store).await;
    reset(&pool, prefix).await;
}

#[tokio::test]
async fn scheduled_delivery_due_on_postgres() {
    let Some(pool) = pool().await else { return };
    let prefix = "t_pg_sched";
    reset(&pool, prefix).await;
    let store = PostgresDispatchStore::with_pool(pool.clone(), prefix)
        .await
        .expect("dispatch");
    harness::assert_scheduled_due(&store).await;
    reset(&pool, prefix).await;
}

#[tokio::test]
async fn dead_letter_budget_on_postgres() {
    let Some(pool) = pool().await else { return };
    let prefix = "t_pg_dlq";
    reset(&pool, prefix).await;
    let store = PostgresDispatchStore::with_pool(pool.clone(), prefix)
        .await
        .expect("dispatch");
    harness::assert_dead_letter(&store).await;
    reset(&pool, prefix).await;
}
