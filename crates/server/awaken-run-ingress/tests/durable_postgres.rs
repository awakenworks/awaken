//! Full durable-ingress end to end on Postgres: the dispatch queue and the
//! commit boundary both persist, sharing one database (distinct scoped bundles).
//!
//! Proves the crown-jewel loop — a durable submit awaits, the dispatch row
//! survives a restart, and a delivered decision wakes and resumes the run to a
//! committed terminal state — against real storage. Skips when no Postgres is
//! reachable.

mod harness;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::{
    PartialToolCall, StreamCheckpoint, StreamCheckpointStore,
};
use awaken_agent_contract::thread::commit::coordinator::{
    Coordinator as CommitCoordinator, Error as CommitError,
};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, RunDisposition, ThreadCommit};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_run_ingress::{
    ClaimedCommitCoordinator, ClaimedRunCommit, DispatchQueue, DispatchTestHarness, DispatchWorker,
    GuardedRunCommit, Inbox, PendingInput, PostgresDispatchStore, PostgresStreamCheckpointStore,
    RunClaim, RunDispatch, SubmitOptions,
};
use awaken_run_ingress_testkit::{AuthoritativeWallClock, ConformanceClock};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_store_postgres::PostgresCommitCoordinator;
use sqlx::Executor as _;

use harness::{
    FailingCommit, THREAD, TICKET, activation, activation_on, blocking_tool_runtime, tool_runtime,
};

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
        ThreadId(THREAD.to_string()),
        RunDisposition::running(RunId(run_id.to_string())),
        true,
        Vec::new(),
        Vec::new(),
        Vec::new(),
    )
}

fn stream_checkpoint(run_id: &str, text: &str) -> StreamCheckpoint {
    StreamCheckpoint {
        run_id: run_id.to_owned(),
        thread_id: "checkpoint-thread".to_owned(),
        model: "provider/model".to_owned(),
        partial_text: text.to_owned(),
        partial_tools: vec![PartialToolCall {
            call_id: "call-1".to_owned(),
            tool_id: "tool-1".to_owned(),
            raw_arguments: "{\"incomplete\":".to_owned(),
        }],
        retry_count: 0,
    }
}

#[tokio::test]
async fn legacy_completion_without_dispatch_identity_fails_closed() {
    // Legacy-tombstone decision rule L1: C1 a completed Run id exists, C2 its
    // pre-V0024 fingerprint is NULL, C3 a caller submits an otherwise plausible
    // dispatch under that id => E1 reject the replay and E2 create no live row.
    // This proves the SQL authority does not infer equality from missing history.
    let Some(pool) = harness::schema_pool("t_pg_legacy_completion_identity").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch store");
    pool.execute(
        "INSERT INTO runtime_dispatch_completion (run_id, request_fingerprint) \
         VALUES ('legacy-completed', NULL)",
    )
    .await
    .expect("insert historical tombstone");

    assert!(
        store
            .enqueue(RunDispatch::new(activation("legacy-completed")))
            .await
            .is_err(),
        "L1/E1"
    );
    assert!(store.list_dispatches().await.unwrap().is_empty(), "L1/E2");
}

#[tokio::test]
async fn postgres_stream_checkpoint_survives_restart_overwrites_and_deletes() {
    let Some(pool) = harness::schema_pool("t_pg_stream_checkpoint").await else {
        return;
    };
    let store = PostgresStreamCheckpointStore::with_pool(pool.clone())
        .await
        .expect("checkpoint store");
    assert_eq!(store.get("run-checkpoint").await.unwrap(), None);

    store
        .put(stream_checkpoint("run-checkpoint", "partial-a"))
        .await
        .unwrap();
    assert_eq!(
        store.get("run-checkpoint").await.unwrap(),
        Some(stream_checkpoint("run-checkpoint", "partial-a"))
    );

    let restarted = PostgresStreamCheckpointStore::with_pool(pool)
        .await
        .expect("restarted checkpoint store");
    restarted
        .put(stream_checkpoint("run-checkpoint", "partial-b"))
        .await
        .unwrap();
    assert_eq!(
        restarted.get("run-checkpoint").await.unwrap(),
        Some(stream_checkpoint("run-checkpoint", "partial-b"))
    );
    restarted.delete("run-checkpoint").await.unwrap();
    restarted.delete("run-checkpoint").await.unwrap();
    assert_eq!(restarted.get("run-checkpoint").await.unwrap(), None);
}

/// Single-writer-per-thread (ADR-0022), topology-independent: two runs of the SAME
/// thread are both pending; two claimers race concurrently. The V0012
/// one-running-per-thread unique index is the backstop the read-committed SELECT
/// guard cannot provide alone — exactly ONE wins, the other loses the unique race
/// (SQLSTATE 23505) and claims nothing. This is the cross-process guarantee that
/// "sole claimer" (single pool) could not give.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_one_running_per_thread_under_concurrent_claimers() {
    let schema = "t_pg_claim_guard";
    if harness::schema_pool(schema).await.is_none() {
        return;
    }
    let store = Arc::new(
        PostgresDispatchStore::connect(&harness::database_url_in_schema(schema), 10)
            .await
            .expect("connect"),
    );
    // Two fresh pending runs on ONE thread (no supersession, so both coexist).
    let fresh = SubmitOptions {
        supersede: false,
        ..Default::default()
    };
    for run in ["cg-1", "cg-2"] {
        store
            .enqueue_with(
                RunDispatch::new(activation_on(run, "cg-thread")),
                fresh.clone(),
            )
            .await
            .expect("enqueue");
    }

    // Race two claimers on separate tasks (separate pool connections).
    let a = store.clone();
    let b = store.clone();
    let (ra, rb) = tokio::join!(
        tokio::spawn(async move { a.claim("owner-a", 1_000, 0, &Default::default()).await }),
        tokio::spawn(async move { b.claim("owner-b", 1_000, 0, &Default::default()).await }),
    );
    let won = ra.unwrap().expect("claim a ok").is_some() as u8
        + rb.unwrap().expect("claim b ok").is_some() as u8;
    assert_eq!(
        won, 1,
        "exactly one of two concurrent same-thread claims wins (single-writer)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_epoch_guard_prevents_reclaim_until_commit_returns() {
    let Some(pool) = harness::schema_pool("t_pg_commit_epoch_guard").await else {
        return;
    };
    let store = Arc::new(
        PostgresDispatchStore::with_pool(pool)
            .await
            .expect("dispatch"),
    );
    store
        .enqueue(RunDispatch::new(activation("guarded")))
        .await
        .unwrap();
    let lease = store
        .claim("owner-a", 100, 0, &Default::default())
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
    AuthoritativeWallClock.advance_past(lease.expires_ms).await;

    let reclaim_store = store.clone();
    let mut reclaiming = tokio::spawn(async move {
        reclaim_store
            .claim("owner-b", 100, 200, &Default::default())
            .await
    });
    // Claim intentionally uses SKIP LOCKED. Depending on scheduling it either
    // waits for the guard's transaction or immediately reports no claim; both
    // are safe, but it must never hand the guarded row to owner-b.
    let early = tokio::time::timeout(std::time::Duration::from_millis(50), &mut reclaiming).await;
    let skipped_locked_row = match early {
        Ok(result) => {
            assert!(
                result.unwrap().unwrap().is_none(),
                "the guarded row cannot be reclaimed before its commit returns"
            );
            true
        }
        Err(_) => false,
    };

    inner.release.notify_one();
    committing.await.unwrap().expect("commit");
    let reclaimed = if skipped_locked_row {
        store
            .claim("owner-b", 100, 200, &Default::default())
            .await
            .unwrap()
            .expect("reclaim after the guard releases")
    } else {
        reclaiming.await.unwrap().unwrap().expect("reclaim")
    };
    assert_eq!(reclaimed.lease.epoch, lease.epoch + 1);
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
async fn durable_submit_awaits_then_delivered_decision_resumes_on_postgres() {
    // Test design. Causes: C1 PostgreSQL is reachable; C2 a submitted Run commits
    // Awaiting; C3 a matching decision is delivered. Effects: E1 C2 persists the
    // ticket without running the tool; E2 C3 resumes once and settles terminal;
    // E3 restart-visible dispatch/input state is cleared. Constraint/Invariant:
    // queue and committed Thread truth remain separate authorities joined by the
    // exact ticket. Decision rule: when C1, exercise C2 before and after C3.
    let Some(pool) = harness::schema_pool("t_e2e").await else {
        return;
    };

    let (runtime, ran) = tool_runtime();
    let commit = Arc::new(
        PostgresCommitCoordinator::with_pool(pool.clone())
            .await
            .expect("commit"),
    );
    let store = Arc::new(
        PostgresDispatchStore::with_pool(pool.clone())
            .await
            .expect("dispatch"),
    );
    let ingress = DispatchTestHarness::new(runtime, store.clone(), commit.clone());

    // Durable submit awaits on the gate; the tool has not run.
    let state = ingress
        .submit_background(activation("run-1"))
        .await
        .expect("submit");
    assert_eq!(state, RunState::Awaiting);
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
                result: ResumeResult::allow(),
                context_messages: Vec::new(),
            },
            harness::clock(0),
        )
        .await
        .expect("resume");
    assert_eq!(resumed, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(ran.load(Ordering::SeqCst), 1);

    // Committed truth is terminal.
    let record =
        CommittedThreadView::run(&*commit, &RunId("run-1".to_string())).expect("run record");
    assert_eq!(record.state, RunState::Ended(EndCause::NaturalEnd));
}

#[tokio::test]
async fn enqueued_dispatch_survives_a_restart() {
    let Some(pool) = harness::schema_pool("t_durable").await else {
        return;
    };

    // Enqueue a run, then drop the store to simulate a process restart.
    {
        let store = PostgresDispatchStore::with_pool(pool.clone())
            .await
            .expect("dispatch a");
        store
            .enqueue(RunDispatch::new(activation("run-1")))
            .await
            .expect("enqueue");
    }

    // A fresh store on the same database still has the accepted run to claim.
    let restarted = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch b");
    let claimed = restarted
        .claim("worker", 1_000, 0, &Default::default())
        .await
        .expect("claim")
        .expect("the enqueued run survived restart");
    assert_eq!(claimed.request.run_id().0, "run-1");
}

#[tokio::test]
async fn postgres_connect_applies_migrations_and_claim_recovers_a_lease() {
    // Cause/effect decision table for schema access:
    // R1 empty schema + connect -> migration bundle is applied.
    // R2 R1 ledger + connect_existing -> verification succeeds without DDL and
    // the resulting store serves the same dispatch behavior.
    // R3 empty schema + connect_existing -> fail closed (covered by the
    // scoped-migration verify tests shared by every Postgres adapter).
    // Create the isolated schema; connect() opens its own pool, so pin its
    // search_path via the URL.
    let schema = "t_pg_recover";
    if harness::schema_pool(schema).await.is_none() {
        return;
    }

    // connect() (not with_pool) applies the dispatch migrations on a fresh pool.
    let url = harness::database_url_in_schema(schema);
    PostgresDispatchStore::connect(&url, 10)
        .await
        .expect("connect");
    let store = PostgresDispatchStore::connect_existing(&url, 10)
        .await
        .expect("verify and connect existing");
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .expect("enqueue");
    // Re-enqueue is idempotent.
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .expect("re-enqueue");

    // Claim with a zero lease, then a later claim reclaims the expired lease.
    assert!(
        store
            .claim("a", 0, 100, &Default::default())
            .await
            .unwrap()
            .is_some()
    );
    let recovered = store
        .claim("b", 1_000, 101, &Default::default())
        .await
        .unwrap();
    assert_eq!(
        recovered.map(|c| c.lease.owner),
        Some("b".to_string()),
        "the expired lease was reclaimed"
    );
}

#[tokio::test]
async fn postgres_append_is_idempotent_and_stale_input_is_dropped() {
    // Test design. Causes: C1 PostgreSQL is reachable; C2 the same message id and
    // payload is appended twice; C3 input carries a stale correlation; C4 matching
    // input follows. Effects: E1 C2 stores once; E2 C3 does not resume; E3 C4
    // resumes once. Constraint/Invariant: message idempotency cannot weaken exact
    // ticket correlation. Decision rule: when C1, cover exact replay plus stale
    // and current correlation partitions.
    let Some(pool) = harness::schema_pool("t_pg_stale").await else {
        return;
    };

    let (runtime, ran) = tool_runtime();
    let commit = Arc::new(
        PostgresCommitCoordinator::with_pool(pool.clone())
            .await
            .expect("commit"),
    );
    let store = Arc::new(
        PostgresDispatchStore::with_pool(pool.clone())
            .await
            .expect("dispatch"),
    );
    let ingress = DispatchTestHarness::new(runtime, store.clone(), commit.clone());

    assert_eq!(
        ingress
            .submit_background(activation("run-1"))
            .await
            .unwrap(),
        RunState::Awaiting
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
    let state = ingress
        .deliver_resume(pending("stale", "old-ticket", true), harness::clock(0))
        .await
        .expect("stale delivery");
    assert_eq!(state, RunState::Awaiting);
    assert_eq!(ran.load(Ordering::SeqCst), 0, "stale input did not resume");

    // The correctly-correlated input (the earlier "dup") now resumes the run.
    let state = ingress
        .deliver_resume(pending("good", TICKET, true), harness::clock(0))
        .await
        .expect("good delivery");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(ran.load(Ordering::SeqCst), 1);
    let record = CommittedThreadView::run(&*commit, &RunId("run-1".to_string())).expect("record");
    assert_eq!(record.state, RunState::Ended(EndCause::NaturalEnd));
}

#[tokio::test]
async fn pending_revision_cas_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_cas").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_pending_revision_cas(&store).await;
}

#[tokio::test]
async fn cross_thread_outbox_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_outbox").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_cross_thread_outbox(&store).await;
}

#[tokio::test]
async fn message_idempotency_conflicts_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_message_idempotency").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool)
        .await
        .expect("dispatch");
    harness::assert_message_idempotency_conflicts(&store).await;
}

#[tokio::test]
async fn scheduled_delivery_due_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_sched").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_scheduled_due(&store, &awaken_run_ingress_testkit::AuthoritativeWallClock)
        .await;
}

#[tokio::test]
async fn millis_boundaries_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_millis_boundaries").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool)
        .await
        .expect("dispatch");
    harness::assert_millis_boundaries(&store, &awaken_run_ingress_testkit::AuthoritativeWallClock)
        .await;
}

#[tokio::test]
async fn dead_letter_budget_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_dlq").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_dead_letter(&store, &awaken_run_ingress_testkit::AuthoritativeWallClock).await;
}

#[tokio::test]
async fn cancel_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_cancel").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_cancel(&store).await;
}

#[tokio::test]
async fn priority_dedupe_gc_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_pdg").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_priority_dedupe_gc(&store, &awaken_run_ingress_testkit::AuthoritativeWallClock)
        .await;
}

#[tokio::test]
async fn lease_renewal_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_renew").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_lease_renewal(&store, &awaken_run_ingress_testkit::AuthoritativeWallClock)
        .await;
}

#[tokio::test]
async fn two_workers_claim_distinct_runs_on_postgres() {
    // Cause graph:
    // C1 = two claims overlap; C2 = the preferred row is locked by the other
    // transaction; C3 = another eligible row exists on a distinct Thread.
    // E1 = the second claim skips the locked row and claims the other row;
    // E2 = no duplicate run id is returned.
    //
    // | Rule | C1 | C2 | C3 | E1 | E2 |
    // | D1   | 0  | 0  | 1  |  - |  1 |
    // | D2   | 1  | 1  | 0  |  0 |  1 |
    // | D3   | 1  | 1  | 1  |  1 |  1 |
    //
    // This test is D3. The ordinary sequential claim tests cover D1 and the
    // empty-queue conformance cases cover D2. Runs use distinct Threads because
    // ADR-0022 intentionally permits only one active run per Thread.
    let Some(pool) = harness::schema_pool("t_pg_multi").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    store
        .enqueue(RunDispatch::new(activation_on("run-1", "thread-1")))
        .await
        .unwrap();
    store
        .enqueue(RunDispatch::new(activation_on("run-2", "thread-2")))
        .await
        .unwrap();

    let capabilities = Default::default();
    let (a, b) = tokio::join!(
        store.claim("wa", 1_000, 0, &capabilities),
        store.claim("wb", 1_000, 0, &capabilities)
    );
    let a = a
        .unwrap()
        .expect("worker a claims")
        .request
        .run_id()
        .0
        .clone();
    let b = b
        .unwrap()
        .expect("worker b claims")
        .request
        .run_id()
        .0
        .clone();
    assert_ne!(a, b, "two concurrent workers claim distinct runs");
}

#[tokio::test]
async fn idle_thread_inbox_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_idle").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_idle_thread_inbox(&store).await;
}

#[tokio::test]
async fn supersession_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_super").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_supersession(&store).await;
}

#[tokio::test]
async fn settle_fences_stale_epoch_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_fence").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_settle_fences_stale_epoch(
        &store,
        &awaken_run_ingress_testkit::AuthoritativeWallClock,
    )
    .await;
}

#[tokio::test]
async fn concurrent_recovery_yields_one_winner_on_postgres() {
    let schema = "t_pg_concurrent_recovery";
    let Some(pool) = harness::schema_pool(schema).await else {
        return;
    };
    let store = Arc::new(
        PostgresDispatchStore::with_pool(pool.clone())
            .await
            .expect("dispatch"),
    );
    harness::assert_concurrent_recovery_yields_one_winner(
        store,
        &awaken_run_ingress_testkit::AuthoritativeWallClock,
    )
    .await;
}

#[tokio::test]
async fn awaiting_settle_fences_stale_epoch_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_fence_await").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_awaiting_settle_fences_stale_epoch(
        &store,
        &awaken_run_ingress_testkit::AuthoritativeWallClock,
    )
    .await;
}

#[tokio::test]
async fn dead_letter_ttl_gc_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_ttlgc").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_dead_letter_ttl_gc(&store, &awaken_run_ingress_testkit::AuthoritativeWallClock)
        .await;
}

#[tokio::test]
async fn relinquish_claim_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_relinquish").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_relinquish_claim(&store).await;
}

#[tokio::test]
async fn physical_attempt_admission_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_physical_attempt").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_physical_attempt_admission(
        &store,
        &awaken_run_ingress_testkit::AuthoritativeWallClock,
    )
    .await;
}

#[tokio::test]
async fn list_dispatches_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_list").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_list_dispatches(&store).await;
}

/// Postgres parity for recovery after a process-bound non-recoverable tool has
/// quiesced (`lease_semantics.rs`). The persisted Executing phase makes the
/// replacement apply NeverReplay, while the claim fence rejects a delayed stale
/// commit and keeps the committed log exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_reclaim_after_process_bound_attempt_quiesces_applies_never_replay_policy() {
    // Test design — two-coordinator active-active history: B opens before A's
    // Running commit, so B's process-start projection is intentionally empty.
    // Each Worker must install an authoritative claim snapshot before reading;
    // B therefore observes Running and applies NeverReplay, while A refreshes
    // terminal truth after losing its fenced commit. No decision may depend on
    // either coordinator's stale process-start projection.
    // Causes: C1 owner A is blocked after committing tool Executing; C2 the
    // release gate lets its executor Future return with an injected commit
    // failure; C3 `run_physical_attempt` records A's exact physical quiescence;
    // C4 its lease then expires; C5 owner B reclaims through a second
    // coordinator. Effects: E1 B reads current Running truth and applies
    // NeverReplay; E2 the external tool runs once; E3 one terminal log survives
    // A's fenced delayed commit. Constraints: every claim installs an
    // authoritative snapshot before policy; only production executor return,
    // never abort/drop, clears the slot. The Memory end-to-end and shared store
    // conformance own the complementary C1+!C2 Blocked rule. Decision rule:
    // execute C1-C5 and assert E1-E3.
    const LEASE: u64 = 1_000;
    let schema = "t_pg_midflight";
    let Some(pool) = harness::schema_pool(schema).await else {
        return;
    };
    let commit_a_inner = Arc::new(
        PostgresCommitCoordinator::with_pool(pool.clone())
            .await
            .expect("commit A"),
    );
    let commit_a = Arc::new(FailingCommit::new(commit_a_inner, false));
    let commit_b = Arc::new(
        PostgresCommitCoordinator::with_existing_pool(pool.clone())
            .await
            .expect("commit B"),
    );
    let store = Arc::new(
        PostgresDispatchStore::with_pool(pool.clone())
            .await
            .expect("dispatch"),
    );

    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (runtime, ran) = blocking_tool_runtime(release.clone());
    let run = RunId("run-1".to_string());
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .expect("enqueue");

    // Owner A claims explicitly, then drives in the background. Retaining the
    // exact lease lets the fixture observe the production return/renewal scope
    // without pretending a task abort is physical quiescence.
    let claimed_a = store
        .claim("owner-a", LEASE, 0, &Default::default())
        .await
        .expect("A claim query")
        .expect("A claims");
    let lease_a = claimed_a.lease.clone();
    let worker_a = Arc::new(
        DispatchWorker::new(runtime.clone(), store.clone(), commit_a.clone(), "owner-a")
            .with_lease_ms(LEASE),
    );
    let a_handle = {
        let worker_a = worker_a.clone();
        tokio::spawn(async move { worker_a.drive_claimed(claimed_a, harness::clock(0)).await })
    };

    // Wait until A is frozen inside the tool.
    let frozen = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while ran.load(Ordering::SeqCst) < 1 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(frozen.is_ok(), "A reached and blocked in the tool");

    let record = CommittedThreadView::run(&*commit_a, &run).expect("A committed a record");
    assert_eq!(
        record.state,
        RunState::Running,
        "A committed a mid-flight Running"
    );

    // Let A's tool return through the real executor, then reject its next commit.
    // The executor Future therefore returns `Err` through `run_physical_attempt`,
    // which is the sole production owner of the exact quiescence ACK. The drive
    // exits and drops its renewal guard without pretending abort/drop is proof.
    commit_a.set_failing(true);
    release.add_permits(1);
    let a_error = tokio::time::timeout(std::time::Duration::from_secs(10), a_handle)
        .await
        .expect("C2 A drive returns")
        .expect("C2 A task does not panic")
        .expect_err("C2 injected commit failure is re-raised");
    assert!(
        a_error.to_string().contains("injected commit failure"),
        "C2 exact drive failure is preserved: {a_error}"
    );
    assert!(
        !store.list_dispatches().await.unwrap()[0].physical_attempt_active,
        "C3 production executor return recorded A's exact physical quiescence"
    );
    commit_a.set_failing(false);
    let renewal_stopped_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("host clock is after the Unix epoch")
        .as_millis() as u64;
    AuthoritativeWallClock
        .advance_past(
            lease_a
                .expires_ms
                .max(renewal_stopped_at_ms.saturating_add(LEASE)),
        )
        .await;

    // Owner B's lease-expired reclaim recovers the committed Executing phase and
    // completes the run without entering the non-recoverable tool again.
    let worker_b = DispatchWorker::new(runtime, store.clone(), commit_b.clone(), "owner-b")
        .with_lease_ms(LEASE);
    let processed = worker_b
        .tick(harness::clock(LEASE + 1))
        .await
        .expect("B drives");
    assert_eq!(
        processed,
        Some((run.clone(), RunState::Ended(EndCause::NaturalEnd))),
        "B reclaimed the still-running run and drove it to completion"
    );

    // A delayed stale write carrying epoch 1 is fenced before it reaches
    // committed Thread truth. This independently preserves the slow/stale
    // message partition after B has advanced the exact mutation claim.
    let stale_service: Arc<dyn ClaimedRunCommit> =
        Arc::new(GuardedRunCommit::new(commit_a.clone(), store.clone()));
    let stale_commit = ClaimedCommitCoordinator::new(stale_service, RunClaim::from(&lease_a));
    assert!(
        stale_commit.commit(running_commit("run-1")).await.is_err(),
        "the stale owner's delayed commit is fenced"
    );

    // The persisted Executing phase prevents an unsafe second invocation.
    assert_eq!(ran.load(Ordering::SeqCst), 1, "the tool ran only once");

    // THE guarantee: exactly-once committed LOG. The committed transcript carries
    // exactly ONE final "all done" assistant message and the run's record is a single
    // terminal fact — the stale owner's duplicate terminal commit was fenced.
    let all_done =
        CommittedThreadView::committed_messages(&*commit_b, &ThreadId(THREAD.to_string()))
            .into_iter()
            .filter(|m| m.text_content().contains("all done"))
            .count();
    assert_eq!(
        all_done, 1,
        "exactly one final assistant message — no duplicate terminal Step"
    );
    let record = CommittedThreadView::run(&*commit_b, &run).expect("terminal record");
    assert_eq!(
        record.state,
        RunState::Ended(EndCause::NaturalEnd),
        "the run has a single terminal record"
    );
}
