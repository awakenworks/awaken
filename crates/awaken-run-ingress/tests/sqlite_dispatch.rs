//! The SQLite dispatch store and a fully-embedded durable loop. SQLite needs no
//! external server, so these always run: store-level claim/lease/idempotency
//! checks, plus an end-to-end durable submit -> park -> resume entirely on SQLite
//! (dispatch queue *and* commit boundary).

mod harness;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::store::run_store::RunStore;
use awaken_run_ingress::{
    DispatchQueue, DurableRunIngress, Inbox, PendingInput, RunExecutionRequest, SqliteDispatchStore,
};
use awaken_runtime::RunIngress;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_store_sqlite::SqliteCommitCoordinator;

use harness::{TICKET, activation, tool_runtime};

fn pending(message_id: &str, correlation: &str, allow: bool) -> PendingInput {
    harness::pending(
        message_id,
        "run-1",
        correlation,
        ResumeResult::Decision { allow, note: None },
    )
}

#[tokio::test]
async fn enqueue_is_idempotent_and_expired_lease_is_reclaimed() {
    let store = SqliteDispatchStore::open_in_memory().expect("open");
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
    let store = SqliteDispatchStore::open_in_memory().expect("open");
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
    let store = Arc::new(SqliteDispatchStore::open_in_memory().expect("dispatch"));
    let commit = Arc::new(SqliteCommitCoordinator::open_in_memory().expect("commit"));
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
    let store = SqliteDispatchStore::open_in_memory().expect("open");
    harness::assert_pending_revision_cas(&store).await;
}

#[tokio::test]
async fn cross_thread_outbox_on_sqlite() {
    let store = SqliteDispatchStore::open_in_memory().expect("open");
    harness::assert_cross_thread_outbox(&store).await;
}

#[tokio::test]
async fn scheduled_delivery_due_on_sqlite() {
    let store = SqliteDispatchStore::open_in_memory().expect("open");
    harness::assert_scheduled_due(&store).await;
}

#[tokio::test]
async fn sqlite_dispatch_opens_a_file_and_persists() {
    let path =
        std::env::temp_dir().join(format!("awaken_sqlite_dispatch_{}.db", std::process::id()));
    let path = path.to_str().unwrap().to_string();
    let _ = std::fs::remove_file(&path);

    {
        let store = SqliteDispatchStore::open(&path).expect("open a");
        store
            .enqueue(RunExecutionRequest::new(activation("run-1")))
            .await
            .unwrap();
    }
    // A fresh handle on the same file still has the enqueued run.
    let restarted = SqliteDispatchStore::open(&path).expect("open b");
    let claimed = restarted
        .claim("w", 1_000, 0)
        .await
        .unwrap()
        .expect("survived");
    assert_eq!(claimed.request.run_id().0, "run-1");
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn dead_letter_budget_on_sqlite() {
    let store = SqliteDispatchStore::open_in_memory().expect("open");
    harness::assert_dead_letter(&store).await;
}

#[tokio::test]
async fn cancel_on_sqlite() {
    let store = SqliteDispatchStore::open_in_memory().expect("open");
    harness::assert_cancel(&store).await;
}

#[tokio::test]
async fn priority_dedupe_gc_on_sqlite() {
    let store = SqliteDispatchStore::open_in_memory().expect("open");
    harness::assert_priority_dedupe_gc(&store).await;
}

#[tokio::test]
async fn lease_renewal_on_sqlite() {
    let store = SqliteDispatchStore::open_in_memory().expect("open");
    harness::assert_lease_renewal(&store).await;
}

#[tokio::test]
async fn idle_thread_inbox_on_sqlite() {
    let store = SqliteDispatchStore::open_in_memory().expect("open");
    harness::assert_idle_thread_inbox(&store).await;
}

#[tokio::test]
async fn supersession_on_sqlite() {
    let store = SqliteDispatchStore::open_in_memory().expect("open");
    harness::assert_supersession(&store).await;
}

#[tokio::test]
async fn dead_letter_ttl_gc_on_sqlite() {
    let store = SqliteDispatchStore::open_in_memory().expect("open");
    harness::assert_dead_letter_ttl_gc(&store).await;
}

#[tokio::test]
async fn renew_owned_leases_on_sqlite() {
    let store = SqliteDispatchStore::open_in_memory().expect("open");
    harness::assert_renew_owned_leases(&store).await;
}

#[tokio::test]
async fn list_dispatches_on_sqlite() {
    let store = SqliteDispatchStore::open_in_memory().expect("open");
    harness::assert_list_dispatches(&store).await;
}
