// @formal evidence ingress.commit_epoch_atomic_local
// @formal evidence run_ingress.relinquish_requires_exact_current_claim_and_preserves_retry_budget
//! The SQLite dispatch store and a fully-embedded durable loop. SQLite needs no
//! external server, so these always run: store-level claim/lease/idempotency
//! checks, plus an end-to-end durable submit -> await -> resume entirely on SQLite
//! (dispatch queue *and* commit boundary).

mod harness;

#[tokio::test]
async fn closed_entry_survives_process_reopen_and_rejects_late_worker_ack() {
    use awaken_dispatch::{
        ControlDispatchQueue, DispatchQueue, SettleOutcome, SqliteDispatchStore,
    };
    // C1 close is committed before its caller loses the response; C2 all
    // process-local handles vanish; C3 an old ACK arrives after cold reopen;
    // C4 independent exact closure becomes available. S1=C1+C2 => closing
    // survives with the original binding. S2=C3 => Fenced, no early release.
    // S3=C4 => one retirement; duplicate closure cannot resurrect the epoch.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("closing.db");
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open(path.to_str().unwrap(), clock.clock()).unwrap();
    let (claim, binding, _) =
        harness::attempt_binding::bound_claim(&store, "cold-closing", &clock).await;
    store.begin_attempt(&claim, Some(&binding)).await.unwrap();
    store.close_attempt(&claim).await.unwrap();
    drop(store);
    let reopened = SqliteDispatchStore::open(path.to_str().unwrap(), clock.clock()).unwrap();
    let stored = reopened
        .load_physical_attempt(&claim.run_id)
        .await
        .unwrap()
        .unwrap();
    assert!(stored.closing);
    assert_eq!(stored.binding, Some(binding));
    assert_eq!(
        reopened.finish_attempt(&claim).await.unwrap(),
        SettleOutcome::Fenced
    );
    assert_eq!(
        reopened.finish_attempt_closure(&claim).await.unwrap(),
        SettleOutcome::Applied
    );
    assert_eq!(
        reopened.finish_attempt_closure(&claim).await.unwrap(),
        SettleOutcome::Applied
    );
    assert!(
        reopened
            .load_physical_attempt(&claim.run_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn sqlite_execution_binding_survives_reopen_without_the_worker() {
    // B11 admitted binding -> all store handles dropped -> reopen => exact
    // provider/Worker/physical coordinates remain. B12 raw SQL cannot clear
    // only the slot; the canonical finish clears slot+binding atomically.
    // B13 persisted malformed binding => observation errors, never false empty.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("bound-attempt.db");
    let path = path.to_str().unwrap();
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open(path, clock.clock()).unwrap();
    let (claim, binding, _) =
        harness::attempt_binding::bound_claim(&store, "reopened-binding", &clock).await;
    clock.set(0);
    store.begin_attempt(&claim, Some(&binding)).await.unwrap();
    drop(store);
    let reopened = SqliteDispatchStore::open(path, clock.clock()).unwrap();
    let expected = Some(awaken_dispatch::PhysicalAttempt {
        closing: false,
        claim: claim.clone(),
        binding: Some(binding.clone()),
        thread_id: Some(awaken_agent_contract::agent::thread::Id(
            "reopened-binding".into(),
        )),
        session_thread_id: None,
    });
    assert_eq!(
        reopened.load_physical_attempt(&claim.run_id).await.unwrap(),
        expected,
        "B11"
    );
    let connection = rusqlite::Connection::open(path).unwrap();
    assert!(
        connection
            .execute(
                "UPDATE runtime_dispatch SET active_attempt_owner=NULL, active_attempt_epoch=NULL",
                []
            )
            .is_err(),
        "B12 orphan binding"
    );
    connection
        .execute(
            "UPDATE runtime_dispatch SET active_attempt_binding='null'",
            [],
        )
        .unwrap();
    assert!(
        reopened.load_physical_attempt(&claim.run_id).await.is_err(),
        "B13 malformed is not absence"
    );
    connection
        .execute(
            "UPDATE runtime_dispatch SET active_attempt_binding=?1",
            [serde_json::to_string(&binding).unwrap()],
        )
        .unwrap();
    assert_eq!(
        reopened.finish_attempt(&claim).await.unwrap(),
        awaken_dispatch::SettleOutcome::Applied
    );
    drop(reopened);
    assert_eq!(
        SqliteDispatchStore::open(path, clock.clock())
            .unwrap()
            .load_physical_attempt(&claim.run_id)
            .await
            .unwrap(),
        None,
        "B12 persisted release"
    );
    assert!(
        connection
            .execute(
                "UPDATE runtime_dispatch SET active_attempt_binding='{}'",
                []
            )
            .is_err(),
        "B12 cannot leave orphan metadata"
    );
}

#[tokio::test]
async fn sqlite_ack_retirement_survives_reopen_and_sql_bypass_is_rejected() {
    // R8 ACK commit then process/store loss => the same claim cannot reenter.
    // R9 bypassing admission SQL => CHECK rejects retired or future watermarks.
    // No fixture-supplied terminal truth or in-process registry survives reopen.
    use awaken_dispatch::{AttemptAdmission as A, SettleOutcome as S};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("retirement.db");
    let path = path.to_str().unwrap();
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open(path, clock.clock()).unwrap();
    store
        .enqueue(RunDispatch::new(harness::activation("retired")))
        .await
        .unwrap();
    clock.set(0);
    let a = store
        .claim("a", 1000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let a = RunClaim::from(&a.lease);
    clock.set(0);
    assert_eq!(store.begin_attempt(&a, None).await.unwrap(), A::Applied);
    assert_eq!(store.finish_attempt(&a).await.unwrap(), S::Applied);
    drop(store);
    let reopened = SqliteDispatchStore::open(path, clock.clock()).unwrap();
    clock.set(0);
    assert_eq!(
        reopened.begin_attempt(&a, None).await.unwrap(),
        A::Fenced,
        "R8"
    );
    assert_eq!(
        reopened.finish_attempt(&a).await.unwrap(),
        S::Applied,
        "lost ACK response"
    );
    let connection = rusqlite::Connection::open(path).unwrap();
    for invalid in [
        "UPDATE runtime_dispatch SET active_attempt_owner = 'a', active_attempt_epoch = retired_attempt_epoch",
        "UPDATE runtime_dispatch SET retired_attempt_epoch = lease_epoch + 1",
        "UPDATE runtime_dispatch SET retired_attempt_epoch = -1",
    ] {
        assert!(connection.execute(invalid, []).is_err(), "R9 {invalid}");
    }
}

use awaken_dispatch_contract::ControlDispatchQueue;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::coordinator::{
    Coordinator as CommitCoordinator, Error as CommitError,
};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, RunDisposition, ThreadCommit};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_dispatch::{
    ClaimedCommitCoordinator, ClaimedRunCommit, DispatchCursor, DispatchOperation,
    DispatchOperationalFeed, DispatchQueue, DispatchTestHarness, GuardedRunCommit, Inbox,
    PendingInput, RunClaim, RunDispatch, SqliteDispatchStore,
};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_store_sqlite::SqliteCommitCoordinator;

use awaken_dispatch_testkit::ConformanceClock as _;
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
        if allow {
            ResumeResult::allow()
        } else {
            ResumeResult::deny(None)
        },
    )
}

#[tokio::test]
async fn enqueue_is_idempotent_and_expired_lease_is_reclaimed() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock()).expect("open");
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
    clock.set(0);
    assert!(
        store
            .claim("a", 1_000, &Default::default())
            .await
            .unwrap()
            .is_some()
    );
    clock.set(500);
    assert!(
        store
            .claim("b", 1_000, &Default::default())
            .await
            .unwrap()
            .is_none()
    );
    clock.set(1_001);
    let recovered = store.claim("b", 1_000, &Default::default()).await.unwrap();
    assert_eq!(recovered.map(|c| c.lease.owner), Some("b".to_string()));
}

#[tokio::test]
async fn sqlite_epoch_guard_blocks_reclaim_until_commit_returns() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = Arc::new(SqliteDispatchStore::open_in_memory(clock.clock()).expect("dispatch"));
    store
        .enqueue(RunDispatch::new(activation("guarded")))
        .await
        .unwrap();
    clock.set(0);
    let lease = store
        .claim("owner-a", 100, &Default::default())
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
    let mut reclaiming = tokio::spawn(async move {
        clock.set(200);
        reclaim_store
            .claim("owner-b", 100, &Default::default())
            .await
    });
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
    let store =
        SqliteDispatchStore::open_in_memory(awaken_lease_contract::system_clock()).expect("open");
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
    // Test design. Causes: C1 SQLite owns dispatch and commit stores; C2 the Run
    // awaits; C3 stale then matching input arrives. Effects: E1 C2 persists
    // Awaiting; E2 stale input does not resume; E3 matching input runs once and
    // settles. Constraint/Invariant: no in-memory authority participates.
    // Decision rule: cover Awaiting, stale-correlation, and exact-correlation
    // partitions through the SQLite adapters.
    let (runtime, ran) = tool_runtime();
    let store = Arc::new(
        SqliteDispatchStore::open_in_memory(awaken_lease_contract::system_clock())
            .expect("dispatch"),
    );
    let commit = Arc::new(SqliteCommitCoordinator::open_in_memory().expect("commit"));
    let ingress = DispatchTestHarness::new(runtime, store.clone(), commit.clone());

    // Durable submit awaits on the gate.
    let state = ingress
        .submit_background(activation("run-1"))
        .await
        .expect("submit");
    assert_eq!(state, RunState::Awaiting);
    assert_eq!(ran.load(Ordering::SeqCst), 0);

    // Stale input (wrong correlation) does not resume.
    let state = ingress
        .deliver_resume(pending("stale", "old-ticket", true), harness::clock(0))
        .await
        .expect("stale");
    assert_eq!(state, RunState::Awaiting);
    assert_eq!(ran.load(Ordering::SeqCst), 0);

    // The correctly-correlated input resumes the run to completion.
    let state = ingress
        .deliver_resume(pending("good", TICKET, true), harness::clock(0))
        .await
        .expect("resume");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(ran.load(Ordering::SeqCst), 1, "the pending tool ran once");

    let record =
        CommittedThreadView::run(commit.as_ref(), &RunId("run-1".to_string())).expect("record");
    assert_eq!(record.state, RunState::Ended(EndCause::NaturalEnd));
}

#[tokio::test]
async fn pending_revision_cas_on_sqlite() {
    let store =
        SqliteDispatchStore::open_in_memory(awaken_lease_contract::system_clock()).expect("open");
    harness::assert_pending_revision_cas(&store).await;
}

#[tokio::test]
async fn cross_thread_outbox_on_sqlite() {
    let store =
        SqliteDispatchStore::open_in_memory(awaken_lease_contract::system_clock()).expect("open");
    harness::assert_cross_thread_outbox(&store).await;
}

#[tokio::test]
async fn message_idempotency_conflicts_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock()).unwrap();
    harness::assert_message_idempotency_conflicts(&store, &clock).await;
}

#[tokio::test]
async fn scheduled_delivery_due_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock()).expect("open");
    harness::assert_scheduled_due(&store, &clock).await;
}

#[tokio::test]
async fn millis_boundaries_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock()).unwrap();
    harness::assert_millis_boundaries(&store, &clock).await;
}

#[tokio::test]
async fn sqlite_dispatch_opens_a_file_and_persists() {
    let path =
        std::env::temp_dir().join(format!("awaken_sqlite_dispatch_{}.db", std::process::id()));
    let path = path.to_str().unwrap().to_string();
    let _ = std::fs::remove_file(&path);

    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    {
        let store = SqliteDispatchStore::open(&path, clock.clock()).expect("open a");
        store
            .enqueue(RunDispatch::new(activation("run-1")))
            .await
            .unwrap();
    }
    // A fresh handle on the same file still has the enqueued run.
    let restarted = SqliteDispatchStore::open(&path, clock.clock()).expect("open b");
    clock.set(0);
    let claimed = restarted
        .claim("w", 1_000, &Default::default())
        .await
        .unwrap()
        .expect("survived");
    assert_eq!(claimed.request.run_id().0, "run-1");
    drop(restarted);

    let feed = SqliteDispatchStore::open(&path, clock.clock()).expect("open feed reader");
    let page = feed
        .events_after(DispatchCursor(0), 10)
        .await
        .expect("backfill persisted operations");
    assert!(matches!(
        page.events.as_slice(),
        [event]
            if matches!(
                &event.operation,
                DispatchOperation::Claimed { claim } if claim.run_id.0 == "run-1"
            )
    ));
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn legacy_completion_without_dispatch_identity_fails_closed() {
    // Legacy-tombstone decision rule L1: C1 a completed Run id exists, C2 its
    // pre-V0024 fingerprint is NULL, C3 a caller submits an otherwise plausible
    // dispatch under that id => E1 reject the replay and E2 create no live row.
    // Absence of the compacted payload cannot become authorization after upgrade.
    let path = std::env::temp_dir().join(format!(
        "awaken_sqlite_legacy_completion_{}.db",
        std::process::id()
    ));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    drop(SqliteDispatchStore::open(&path, awaken_lease_contract::system_clock()).expect("migrate"));
    let connection = rusqlite::Connection::open(&path).expect("open legacy fixture");
    connection
        .execute(
            "INSERT INTO runtime_dispatch_completion (run_id, request_fingerprint) \
             VALUES (?1, NULL)",
            ["legacy-completed"],
        )
        .expect("insert historical tombstone");
    drop(connection);

    let store = SqliteDispatchStore::open(&path, awaken_lease_contract::system_clock())
        .expect("reopen migrated store");
    assert!(
        store
            .enqueue(RunDispatch::new(activation("legacy-completed")))
            .await
            .is_err(),
        "L1/E1"
    );
    assert!(store.list_dispatches().await.unwrap().is_empty(), "L1/E2");
    drop(store);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn cancellation_intent_survives_restart_and_reconciles_to_terminal() {
    // Test design. Causes: C1 SQLite persists cancellation intent; C2 the process
    // drops and reopens both stores; C3 recovery drains the intent. Effects: E1
    // C2 retains cancellation; E2 C3 commits Cancelled and removes the dispatch.
    // Constraint/Invariant: restart cannot lose or execute past durable cancel.
    // Decision rule: persist, reopen, recover, and assert terminal truth plus an
    // empty operational queue.
    let base = std::env::temp_dir().join(format!(
        "awaken_sqlite_cancel_recovery_{}_{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let dispatch_path = base.with_extension("dispatch.db");
    let commit_path = base.with_extension("commit.db");
    let dispatch_path = dispatch_path.to_string_lossy().into_owned();
    let commit_path = commit_path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&dispatch_path);
    let _ = std::fs::remove_file(&commit_path);
    let run = RunId("cancel-after-restart".to_string());

    {
        let store =
            SqliteDispatchStore::open(&dispatch_path, awaken_lease_contract::system_clock())
                .expect("first process store");
        store
            .enqueue(RunDispatch::new(activation("cancel-after-restart")))
            .await
            .unwrap();
        assert!(store.cancel(&run).await.unwrap().is_some());
        // Process crashes here: no terminal commit and no settlement.
    }

    let store = Arc::new(
        SqliteDispatchStore::open(&dispatch_path, awaken_lease_contract::system_clock())
            .expect("restarted store"),
    );
    let commit = Arc::new(SqliteCommitCoordinator::open(&commit_path).expect("commit store"));
    let ingress = DispatchTestHarness::new(harness::text_runtime(), store.clone(), commit.clone());
    assert_eq!(
        ingress
            .recover(harness::clock(0))
            .await
            .expect("reconcile cancellation"),
        vec![(run.clone(), RunState::Ended(EndCause::Cancelled))]
    );
    assert!(store.list_dispatches().await.unwrap().is_empty());
    assert_eq!(
        CommittedThreadView::run(commit.as_ref(), &run)
            .expect("terminal record")
            .state,
        RunState::Ended(EndCause::Cancelled)
    );
    drop(ingress);
    drop(store);
    drop(commit);
    let _ = std::fs::remove_file(&dispatch_path);
    let _ = std::fs::remove_file(&commit_path);
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
    let store =
        SqliteDispatchStore::open(&path, awaken_lease_contract::system_clock()).expect("migrate");
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
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock()).expect("open");
    harness::assert_dead_letter(&store, &clock).await;
}

#[tokio::test]
async fn cancel_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock()).expect("open");
    harness::assert_cancel(&store, &clock).await;
}

#[tokio::test]
async fn priority_dedupe_gc_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock()).expect("open");
    harness::assert_priority_dedupe_gc(&store, &clock).await;
}

#[tokio::test]
async fn lease_renewal_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock()).expect("open");
    harness::assert_lease_renewal(&store, &clock).await;
}

#[tokio::test]
async fn idle_thread_inbox_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock()).expect("open");
    harness::assert_idle_thread_inbox(&store, &clock).await;
}

#[tokio::test]
async fn supersession_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock()).expect("open");
    harness::assert_supersession(&store, &clock).await;
}

#[tokio::test]
async fn concurrent_recovery_yields_one_winner_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store =
        std::sync::Arc::new(SqliteDispatchStore::open_in_memory(clock.clock()).expect("open"));
    harness::assert_concurrent_recovery_yields_one_winner(store, &clock).await;
}

#[tokio::test]
async fn dead_letter_ttl_gc_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock()).expect("open");
    harness::assert_dead_letter_ttl_gc(&store, &clock).await;
}

#[tokio::test]
async fn relinquish_claim_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock()).expect("open");
    harness::assert_relinquish_claim(&store, &clock).await;
}

#[tokio::test]
async fn defer_claim_is_request_local_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock()).expect("open");
    harness::assert_defer_claim_is_request_local(&store, &clock).await;
}

#[tokio::test]
async fn defer_claim_uses_the_store_clock_and_exact_deadline_on_sqlite() {
    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(100));
    let store = SqliteDispatchStore::open_in_memory(clock.clone()).expect("open");
    harness::assert_defer_claim_deadline_edge(&store, &clock).await;
}

#[tokio::test]
async fn physical_attempt_admission_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock()).expect("open");
    harness::assert_physical_attempt_admission(&store, &clock).await;
}

#[tokio::test]
async fn list_dispatches_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock()).expect("open");
    harness::assert_list_dispatches(&store, &clock).await;
}

#[tokio::test]
async fn dedupe_ignores_dead_lettered_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock()).expect("open");
    harness::assert_dedupe_ignores_dead_lettered(&store, &clock).await;
}

#[tokio::test]
async fn wake_suppressed_while_thread_running_on_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = SqliteDispatchStore::open_in_memory(clock.clock()).expect("open");
    harness::assert_wake_suppressed_while_thread_running(&store, &clock).await;
}
