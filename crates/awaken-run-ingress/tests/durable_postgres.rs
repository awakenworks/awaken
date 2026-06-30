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
    DurableRunIngress, PendingInput, PostgresDispatchStore, RunDispatch, RunExecutionRequest,
};
use awaken_runtime::RunIngress;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_store_postgres::PostgresCommitCoordinator;

use harness::{THREAD, activation, pool, reset, tool_runtime};

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
