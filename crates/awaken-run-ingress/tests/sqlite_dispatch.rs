//! The SQLite dispatch store and a fully-embedded durable loop. SQLite needs no
//! external server, so these always run: store-level claim/lease/idempotency
//! checks, plus an end-to-end durable submit -> park -> resume entirely on SQLite
//! (dispatch queue *and* commit boundary).

mod harness;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::store::run_store::RunStore;
use awaken_run_ingress::{
    DurableRunIngress, PendingInbox, PendingInput, RunDispatch, RunExecutionRequest,
    SqliteDispatchStore,
};
use awaken_runtime::RunIngress;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_store_sqlite::SqliteCommitCoordinator;

use harness::{THREAD, TICKET, activation, tool_runtime};

fn pending(message_id: &str, correlation: &str, allow: bool) -> PendingInput {
    PendingInput {
        message_id: message_id.to_string(),
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId(THREAD.to_string()),
        correlation_id: correlation.to_string(),
        result: ResumeResult::Decision { allow, note: None },
    }
}

#[tokio::test]
async fn enqueue_is_idempotent_and_expired_lease_is_reclaimed() {
    let store = SqliteDispatchStore::open_in_memory("disp").expect("open");
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    // Re-enqueue is a no-op.
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();

    // A held lease blocks a second claim; an expired lease is reclaimed.
    assert!(store.claim("a", 1_000, 0).await.unwrap().is_some());
    assert!(store.claim("b", 1_000, 500).await.unwrap().is_none());
    let recovered = store.claim("b", 1_000, 1_001).await.unwrap();
    assert_eq!(recovered.map(|c| c.lease.owner), Some("b".to_string()));
}

#[tokio::test]
async fn append_is_idempotent() {
    let store = SqliteDispatchStore::open_in_memory("disp").expect("open");
    let input = pending("msg-1", TICKET, true);
    assert!(
        store.append(input.clone()).await.unwrap(),
        "first append stores"
    );
    assert!(
        !store.append(input).await.unwrap(),
        "a duplicate message id is a no-op"
    );
}

#[tokio::test]
async fn durable_loop_runs_entirely_on_sqlite() {
    let (runtime, ran) = tool_runtime();
    let store = Arc::new(SqliteDispatchStore::open_in_memory("disp").expect("dispatch"));
    let commit = Arc::new(SqliteCommitCoordinator::open_in_memory("rt").expect("commit"));
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());

    // Durable submit parks on the gate.
    let phase = ingress
        .submit_background(activation("run-1"))
        .await
        .expect("submit");
    assert_eq!(phase, Phase::Waiting);
    assert_eq!(ran.load(Ordering::SeqCst), 0);

    // Stale input (wrong correlation) does not resume.
    let phase = ingress
        .deliver_resume(pending("stale", "old-ticket", true), 0)
        .await
        .expect("stale");
    assert_eq!(phase, Phase::Waiting);
    assert_eq!(ran.load(Ordering::SeqCst), 0);

    // The correctly-correlated input resumes the run to completion.
    let phase = ingress
        .deliver_resume(pending("good", TICKET, true), 0)
        .await
        .expect("resume");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    assert_eq!(ran.load(Ordering::SeqCst), 1, "the pending tool ran once");

    let record = RunStore::get(commit.as_ref(), &RunId("run-1".to_string())).expect("record");
    assert_eq!(record.phase, Phase::Ended(EndCause::NaturalEnd));
}

#[tokio::test]
async fn pending_revision_cas_on_sqlite() {
    let store = SqliteDispatchStore::open_in_memory("disp").expect("open");
    harness::assert_pending_revision_cas(&store).await;
}
