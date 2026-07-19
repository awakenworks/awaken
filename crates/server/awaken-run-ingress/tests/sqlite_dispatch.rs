//! The SQLite dispatch store and a fully-embedded durable loop. SQLite needs no
//! external server, so these always run: store-level claim/lease/idempotency
//! checks, plus an end-to-end durable submit -> await -> resume entirely on SQLite
//! (dispatch queue *and* commit boundary).

mod harness;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::coordinator::{
    Coordinator as CommitCoordinator, Error as CommitError,
};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, RunDisposition, ThreadCommit};
use awaken_agent_contract::thread::read::run_store::RunStore;
use awaken_run_ingress::{
    ClaimedCommitCoordinator, ClaimedRunCommit, DispatchQueue, DurableRunIngress, GuardedRunCommit,
    Inbox, PendingInput, RunClaim, RunDispatch, SqliteDispatchStore,
};
use awaken_runtime::RunIngress;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_store_sqlite::SqliteCommitCoordinator;

use harness::{TICKET, activation, tool_runtime};

struct BlockingCommit {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl CommitCoordinator for BlockingCommit {
    async fn commit(&self, _commit: ThreadCommit) -> Result<CommitRecord, CommitError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(CommitRecord { sequence: 1 })
    }
}

fn running_commit(run_id: &str) -> ThreadCommit {
    ThreadCommit::assemble(
        ThreadId(harness::THREAD.to_string()),
        RunDisposition::running(RunId(run_id.to_string())),
        true,
        Vec::new(),
        Vec::new(),
        Vec::new(),
    )
}

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
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    // Re-enqueue is a no-op.
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();

    // A held lease blocks a second claim; an expired lease is reclaimed.
    assert!(store.claim("a", 1_000, 0).await.unwrap().is_some());
    assert!(store.claim("b", 1_000, 500).await.unwrap().is_none());
    let recovered = store.claim("b", 1_000, 1_001).await.unwrap();
    assert_eq!(recovered.map(|c| c.lease.owner), Some("b".to_string()));
}

#[tokio::test]
async fn sqlite_epoch_guard_blocks_reclaim_until_commit_returns() {
    let store = Arc::new(SqliteDispatchStore::open_in_memory().expect("dispatch"));
    store
        .enqueue(RunDispatch::new(activation("guarded")))
        .await
        .unwrap();
    let lease = store
        .claim("owner-a", 100, 0)
        .await
        .unwrap()
        .expect("claim")
        .lease;
    let inner = Arc::new(BlockingCommit {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let service: Arc<dyn ClaimedRunCommit> =
        Arc::new(GuardedRunCommit::new(inner.clone(), store.clone()));
    let fenced = ClaimedCommitCoordinator::new(service, RunClaim::from(&lease));

    let committing = tokio::spawn(async move { fenced.commit(running_commit("guarded")).await });
    inner.entered.notified().await;

    let reclaim_store = store.clone();
    let mut reclaiming =
        tokio::spawn(async move { reclaim_store.claim("owner-b", 100, 200).await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), &mut reclaiming)
            .await
            .is_err(),
        "reclaim must wait while the exact commit epoch is held"
    );

    inner.release.notify_one();
    committing.await.unwrap().expect("commit");
    let reclaimed = reclaiming.await.unwrap().unwrap().expect("reclaim");
    assert_eq!(reclaimed.lease.epoch, lease.epoch + 1);
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

    // Durable submit awaits on the gate.
    let state = ingress
        .submit_background(activation("run-1"))
        .await
        .expect("submit");
    assert_eq!(state, RunState::Awaiting);
    assert_eq!(ran.load(Ordering::SeqCst), 0);

    // Stale input (wrong correlation) does not resume.
    let state = ingress
        .deliver_resume(pending("stale", "old-ticket", true), 0)
        .await
        .expect("stale");
    assert_eq!(state, RunState::Awaiting);
    assert_eq!(ran.load(Ordering::SeqCst), 0);

    // The correctly-correlated input resumes the run to completion.
    let state = ingress
        .deliver_resume(pending("good", TICKET, true), 0)
        .await
        .expect("resume");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(ran.load(Ordering::SeqCst), 1, "the pending tool ran once");

    let record = RunStore::get(commit.as_ref(), &RunId("run-1".to_string())).expect("record");
    assert_eq!(record.state, RunState::Ended(EndCause::NaturalEnd));
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
            .enqueue(RunDispatch::new(activation("run-1")))
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

#[test]
fn latest_schema_removes_the_obsolete_delegation_table() {
    use rusqlite::OptionalExtension as _;

    let path = std::env::temp_dir().join(format!(
        "awaken_sqlite_no_delegation_store_{}.db",
        std::process::id()
    ));
    let path = path.to_str().unwrap().to_string();
    let _ = std::fs::remove_file(&path);
    let store = SqliteDispatchStore::open(&path).expect("migrate");
    drop(store);

    let connection = rusqlite::Connection::open(&path).expect("inspect");
    let table: Option<String> = connection
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'runtime_delegation_group'",
            [],
            |row| row.get(0),
        )
        .optional()
        .expect("query schema");
    assert!(table.is_none(), "delegation state belongs to ThreadCommit");
    drop(connection);
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
async fn settle_fences_stale_epoch_on_sqlite() {
    let store = SqliteDispatchStore::open_in_memory().expect("open");
    harness::assert_settle_fences_stale_epoch(&store).await;
}

#[tokio::test]
async fn concurrent_recovery_yields_one_winner_on_sqlite() {
    let store = std::sync::Arc::new(SqliteDispatchStore::open_in_memory().expect("open"));
    harness::assert_concurrent_recovery_yields_one_winner(store).await;
}

#[tokio::test]
async fn awaiting_settle_fences_stale_epoch_on_sqlite() {
    let store = SqliteDispatchStore::open_in_memory().expect("open");
    harness::assert_awaiting_settle_fences_stale_epoch(&store).await;
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
async fn renew_skips_far_from_expiry_on_sqlite() {
    let store = SqliteDispatchStore::open_in_memory().expect("open");
    harness::assert_renew_skips_far_from_expiry(&store).await;
}

#[tokio::test]
async fn list_dispatches_on_sqlite() {
    let store = SqliteDispatchStore::open_in_memory().expect("open");
    harness::assert_list_dispatches(&store).await;
}

#[tokio::test]
async fn dedupe_ignores_dead_lettered_on_sqlite() {
    let store = SqliteDispatchStore::open_in_memory().expect("open");
    harness::assert_dedupe_ignores_dead_lettered(&store).await;
}

#[tokio::test]
async fn wake_suppressed_while_thread_running_on_sqlite() {
    let store = SqliteDispatchStore::open_in_memory().expect("open");
    harness::assert_wake_suppressed_while_thread_running(&store).await;
}
