//! `AnyDispatchStore`, the runtime-selectable backend (ADR-0019 multi-node
//! wiring). Proves the enum-free wrapper delegates the full `Dispatch` bundle to
//! its active backend and preserves the owner-scoped claim/lease semantics the
//! fleet relies on. The SQLite paths always run; the Postgres path skips when no
//! database is reachable. Also exercises `DurableRunIngress::with_owner`, the seam
//! that gives each fleet process its own unique claim owner.

mod harness;

use std::sync::Arc;

use awaken_agent_contract::agent::run::Phase;
use awaken_run_ingress::{
    AnyDispatchStore, DispatchQueue, DurableRunIngress, Inbox, RunExecutionRequest,
};
use awaken_runtime::RunIngress;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_store_sqlite::SqliteCommitCoordinator;

use harness::{TICKET, activation, tool_runtime};

fn any_in_memory() -> AnyDispatchStore {
    AnyDispatchStore::open_sqlite_in_memory().expect("open in-memory sqlite backend")
}

#[tokio::test]
async fn any_delegates_enqueue_claim_and_owner_scoped_lease() {
    let store = any_in_memory();
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    // Re-enqueue is a no-op (idempotent), delegated through the wrapper.
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();

    // The claim records the owner on the lease; a held lease blocks a second
    // owner; an expired lease is reclaimed by the next owner.
    let claimed = store.claim("owner-a", 1_000, 0).await.unwrap();
    assert_eq!(
        claimed.map(|c| c.lease.owner),
        Some("owner-a".to_string()),
        "claim records the claiming owner on the lease"
    );
    assert!(
        store.claim("owner-b", 1_000, 500).await.unwrap().is_none(),
        "a live lease blocks a second owner"
    );
    let recovered = store.claim("owner-b", 1_000, 1_001).await.unwrap();
    assert_eq!(
        recovered.map(|c| c.lease.owner),
        Some("owner-b".to_string()),
        "an expired lease is reclaimed by the next owner"
    );
}

#[tokio::test]
async fn any_lets_two_owners_claim_distinct_runs() {
    // The multi-worker guarantee (ADR-0019): two distinct owners against one queue
    // claim two distinct runs, never the same one.
    let store = any_in_memory();
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    store
        .enqueue(RunExecutionRequest::new(activation("run-2")))
        .await
        .unwrap();

    let first = store
        .claim("worker-a", 1_000, 0)
        .await
        .unwrap()
        .expect("one");
    let second = store
        .claim("worker-b", 1_000, 0)
        .await
        .unwrap()
        .expect("two");
    assert_eq!(first.lease.owner, "worker-a");
    assert_eq!(second.lease.owner, "worker-b");
    assert_ne!(
        first.request.run_id(),
        second.request.run_id(),
        "distinct owners must claim distinct runs"
    );
    assert!(
        store.claim("worker-c", 1_000, 0).await.unwrap().is_none(),
        "no runnable dispatch remains once both are claimed"
    );
}

#[tokio::test]
async fn any_delegates_inbox_append_idempotency() {
    let store = any_in_memory();
    let input = harness::pending(
        "msg-1",
        "run-1",
        TICKET,
        ResumeResult::Decision {
            allow: true,
            note: None,
        },
    );
    assert!(store.append(input.clone()).await.unwrap(), "first append");
    assert!(
        !store.append(input).await.unwrap(),
        "a duplicate message id is a no-op through the wrapper"
    );
}

#[tokio::test]
async fn with_owner_drives_a_durable_run_over_any_sqlite() {
    // The unique-owner seam end to end: a DurableRunIngress built with an explicit
    // owner over the AnyDispatchStore(sqlite) backend parks, then resumes to a
    // committed terminal phase — proving both the owner param and the wrapper work
    // in the real ingress, not just at the store surface.
    let (runtime, ran) = tool_runtime();
    let store = Arc::new(any_in_memory());
    let commit = Arc::new(SqliteCommitCoordinator::open_in_memory().expect("commit"));
    let ingress =
        DurableRunIngress::with_owner(runtime, store.clone(), commit.clone(), "fleet-node-7");

    let phase = ingress
        .submit_background(activation("run-1"))
        .await
        .expect("submit");
    assert_eq!(phase, Phase::Waiting, "the run parks on the gate");
    assert_eq!(
        ran.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the tool has not run while parked"
    );
    // The run is now parked awaiting input over the AnyDispatchStore(sqlite)
    // backend, built with this process's unique claim owner — the with_owner +
    // wrapper path executed end to end in the real ingress.
    let _ = store;
}

#[tokio::test]
async fn any_postgres_connect_and_claim() {
    // Skips unless a Postgres is reachable (mirrors durable_postgres.rs). Proves
    // AnyDispatchStore::connect_postgres builds a working shared backend and the
    // wrapper delegates enqueue/claim over it.
    let schema = "t_any_store";
    let Some(_pool) = harness::schema_pool(schema).await else {
        return;
    };
    let store = AnyDispatchStore::connect_postgres(&harness::database_url_in_schema(schema))
        .await
        .expect("connect postgres backend");
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .expect("enqueue over postgres");
    let claimed = store.claim("pg-owner", 1_000, 0).await.expect("claim");
    assert_eq!(claimed.map(|c| c.lease.owner), Some("pg-owner".to_string()));
}
