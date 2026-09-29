// @formal evidence checkpoint.crash_preserves_durable_truth
// @formal evidence checkpoint.terminal_completion_clears_resume
// @formal evidence delegation.independent_child_scheduling_lease
// @formal evidence ingress.commit_epoch_atomic_local
// @formal evidence ingress.epoch_reclaim
// @formal evidence ingress.pending_mutation_plan_requires_exact_revision_and_nonwrapping_edit
// @formal evidence ingress.single_owner
// @formal evidence run_ingress.relinquish_requires_exact_current_claim_and_preserves_retry_budget
// @formal evidence session_execution.one_physical_attempt_per_thread
//! Full durable-ingress end to end on Postgres: the dispatch queue and the
//! commit boundary both persist, sharing one database (distinct scoped bundles).
//!
//! Proves the crown-jewel loop — a durable submit awaits, the dispatch row
//! survives a restart, and a delivered decision wakes and resumes the run to a
//! committed terminal state — against real storage. Skips when no Postgres is
//! reachable.

mod harness;

use awaken_lease_contract::system_clock;
use awaken_dispatch_contract::ControlDispatchQueue;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::observation::Observed;
use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
use awaken_agent_contract::thread::commit::coordinator::{
    Coordinator as CommitCoordinator, Error as CommitError,
};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, RunDisposition, ThreadCommit};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_dispatch::{
    ClaimedCommitCoordinator, ClaimedRunCommit, DEFAULT_LEASE_MS, DispatchQueue,
    DispatchTestHarness, DispatchWorker, GuardedRunCommit, Inbox, LeastLoadedPolicy, PendingInput,
    PlacementRequirements, PostgresDispatchStore, PostgresStreamCheckpointStore, RunClaim,
    RunDispatch, SubmitOptions,
};
use awaken_dispatch_testkit::{AuthoritativeWallClock, ConformanceClock};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_store_postgres::PostgresCommitCoordinator;
use sqlx::Executor as _;
use sqlx::postgres::PgPoolOptions;

use harness::{
    FailingCommit, THREAD, TICKET, activation, activation_on,
    blocking_tool_runtime_with_entry_signal, tool_runtime,
};

struct BlockingCommit {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[tokio::test]
async fn postgres_pending_mutations_lock_revision_before_effect_and_preserve_exhaustion() {
    // Cause/effect graph: P1 a third transaction holds this exact pending row;
    // P2 two independent one-connection replicas edit it, or edit/retract it;
    // P3 both real backend pids reach a Lock wait before P1 releases; P4 the
    // row later reaches MAX BIGINT. R1=P1+P2+P3=>one Applied and exact winner;
    // R2=P4+edit=>typed rejection, unchanged row; R3=P4+retract=>remove.
    // This forces the former SELECT->UPDATE gap: without FOR UPDATE both
    // commands read the old revision before waiting and both falsely Apply.
    const SCHEMA: &str = "t_known_defect_pending_cas_20260927";
    let Some(pool) = harness::schema_pool(SCHEMA).await else {
        return;
    };
    let primary = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .unwrap();
    let url = harness::database_url_in_schema(SCHEMA);
    let left_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let right_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let left_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&left_pool)
        .await
        .unwrap();
    let right_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&right_pool)
        .await
        .unwrap();
    let left = Arc::new(PostgresDispatchStore::with_pool(left_pool).await.unwrap());
    let right = Arc::new(PostgresDispatchStore::with_pool(right_pool).await.unwrap());
    for retract in [false, true] {
        let message = if retract {
            "edit-retract-lock"
        } else {
            "edit-edit-lock"
        };
        primary
            .append(harness::pending(
                message,
                "run",
                TICKET,
                ResumeResult::Input("initial".into()),
            ))
            .await
            .unwrap();
        let mut holder = pool.begin().await.unwrap();
        sqlx::query("SELECT revision FROM runtime_pending WHERE message_id = $1 FOR UPDATE")
            .bind(message)
            .fetch_one(&mut *holder)
            .await
            .unwrap();
        let editor = left.clone();
        let edited = tokio::spawn(async move {
            editor
                .edit(message, 1, ResumeResult::Input("left".into()))
                .await
        });
        let mutator = right.clone();
        let changed = tokio::spawn(async move {
            if retract {
                mutator.retract(message, 1).await
            } else {
                mutator
                    .edit(message, 1, ResumeResult::Input("right".into()))
                    .await
            }
        });
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity WHERE pid = ANY($1) AND wait_event_type = 'Lock'",
            ).bind(vec![left_pid, right_pid]).fetch_one(&pool).await.unwrap();
            if waiting == 2 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "P3 both exact replica backend pids must reach row-lock wait"
            );
            // awaken-wait: wait for the two exact PostgreSQL backend pids to
            // report their row-lock wait; the deadline above rejects expiry.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        holder.commit().await.unwrap();
        let (edited, changed) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            (
                edited.await.unwrap().unwrap(),
                changed.await.unwrap().unwrap(),
            )
        })
        .await
        .expect("R1 row mutations finish after the holder commits");
        use awaken_dispatch::CasOutcome;
        assert_eq!(
            usize::from(edited == CasOutcome::Applied)
                + usize::from(changed == CasOutcome::Applied),
            1,
            "R1 one linearized mutation, retract={retract}"
        );
        let rows = primary.list(&ThreadId(THREAD.into())).await.unwrap();
        let row = rows.iter().find(|row| row.input.message_id == message);
        if retract && changed == CasOutcome::Applied {
            assert!(row.is_none(), "R1 exact retract winner");
            assert_eq!(edited, CasOutcome::NotFound, "R1 rejected absent edit");
        } else {
            let row = row.unwrap();
            assert_eq!(row.revision, 2, "R1 exactly one successor");
            assert_eq!(
                row.input.result,
                ResumeResult::Input(
                    if edited == CasOutcome::Applied {
                        "left"
                    } else {
                        "right"
                    }
                    .into()
                ),
                "R1 exact winner bytes"
            );
        }
    }
    primary
        .append(harness::pending(
            "pg-revision-limit",
            "run",
            TICKET,
            ResumeResult::Input("old".into()),
        ))
        .await
        .unwrap();
    sqlx::query("UPDATE runtime_pending SET revision = $1 WHERE message_id = 'pg-revision-limit'")
        .bind(i64::MAX)
        .execute(&pool)
        .await
        .unwrap();
    let before = primary.list(&ThreadId(THREAD.into())).await.unwrap();
    assert!(
        primary
            .edit(
                "pg-revision-limit",
                i64::MAX as u64,
                ResumeResult::Input("new".into())
            )
            .await
            .is_err(),
        "R2"
    );
    assert_eq!(
        primary.list(&ThreadId(THREAD.into())).await.unwrap(),
        before,
        "R2 durable frame unchanged"
    );
    assert_eq!(
        primary
            .retract("pg-revision-limit", i64::MAX as u64)
            .await
            .unwrap(),
        awaken_dispatch::CasOutcome::Applied,
        "R3"
    );
}

#[tokio::test]
async fn postgres_execution_binding_is_frozen_with_the_physical_slot() {
    // B1-B9 use PostgreSQL's authoritative clock, not the caller's clock.
    // B10 raw SQL cannot leave binding metadata on an unoccupied row.
    let Some(pool) = harness::schema_pool("t_pg_attempt_binding").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .unwrap();
    harness::attempt_binding::assert_binding_lifecycle(
        &store,
        &awaken_dispatch_testkit::AuthoritativeWallClock,
    )
    .await;
    harness::attempt_binding::assert_closing_lifecycle(
        &store,
        &awaken_dispatch_testkit::AuthoritativeWallClock,
    )
    .await;
    let error = sqlx::query(
        "UPDATE runtime_dispatch SET active_attempt_binding = '{}' WHERE run_id = 'bound-attempt'",
    )
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("23514"),
        "B10"
    );
}

#[tokio::test]
async fn postgres_returned_attempt_epoch_is_not_reused() {
    // Execute shared R1-R6 with PostgreSQL's own transaction clock. An explicit
    // AWAKEN_TEST_DATABASE_URL makes database unavailability fail this gate.
    let Some(pool) = harness::schema_pool("t_pg_attempt_retirement").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .unwrap();
    harness::attempt_retirement::assert_retirement(
        &store,
        0,
        &awaken_dispatch_testkit::AuthoritativeWallClock,
    )
    .await;
    // R7 SQL bypass cannot reinstall an already retired epoch, even if a caller
    // skips the application reducer. The named row belongs only to this test.
    let error = sqlx::query("UPDATE runtime_dispatch SET active_attempt_owner = 'stale', active_attempt_epoch = retired_attempt_epoch WHERE run_id = 'retirement'")
        .execute(&pool).await.unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("23514")
    );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_physical_thread_constraint_survives_bypassed_admission() {
    // Cause/effect table: C1 two Pending Runs share a Thread; C2 independent
    // connections bypass application admission and concurrently write physical
    // owners. R1 => exactly one write succeeds, one SQLSTATE 23505, one owner.
    // R2 another Thread => succeeds independently. R3 exact predecessor clears
    // => the formerly excluded Run may occupy the Thread. R4 half a pair =>
    // rejected, preserving the existing pair constraint. Database constraints
    // are the final backstop, not the source of ordinary Blocked responses.
    let Some(pool) = harness::schema_pool("t_pg_physical_thread_constraint").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .unwrap();
    for (run, thread) in [("pa", "pt"), ("pb", "pt"), ("pc", "other-pt")] {
        store
            .enqueue_with(
                RunDispatch::new(activation_on(run, thread)),
                SubmitOptions {
                    supersede: false,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    let mut a = tokio::time::timeout(std::time::Duration::from_secs(30), pool.acquire())
        .await
        .expect("timed out waiting for pool.acquire()")
        .unwrap();
    let mut b = tokio::time::timeout(std::time::Duration::from_secs(30), pool.acquire())
        .await
        .expect("timed out waiting for pool.acquire()")
        .unwrap();
    let write = "UPDATE runtime_dispatch SET active_attempt_owner = run_id, active_attempt_epoch = 1 WHERE run_id = $1";
    let (ra, rb) = tokio::join!(
        sqlx::query(write).bind("pa").execute(&mut *a),
        sqlx::query(write).bind("pb").execute(&mut *b),
    );
    assert_eq!(usize::from(ra.is_ok()) + usize::from(rb.is_ok()), 1, "R1");
    let (winner, loser, error) = match (ra, rb) {
        (Ok(_), Err(error)) => ("pa", "pb", error),
        (Err(error), Ok(_)) => ("pb", "pa", error),
        _ => panic!("R1 exactly one physical owner must win"),
    };
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("23505"),
        "R1"
    );
    drop(a);
    drop(b);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runtime_dispatch WHERE thread_id = 'pt' AND active_attempt_owner IS NOT NULL")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(count, 1, "R1");
    sqlx::query(write).bind("pc").execute(&pool).await.unwrap();
    sqlx::query("UPDATE runtime_dispatch SET active_attempt_owner = NULL, active_attempt_epoch = NULL WHERE run_id = $1")
        .bind(winner).execute(&pool).await.unwrap();
    sqlx::query(write).bind(loser).execute(&pool).await.unwrap();
    assert!(
        sqlx::query("UPDATE runtime_dispatch SET active_attempt_epoch = NULL WHERE run_id = $1")
            .bind(loser)
            .execute(&pool)
            .await
            .is_err(),
        "R4"
    );
}

#[tokio::test]
async fn postgres_stream_checkpoint_survives_restart_overwrites_and_deletes() {
    let Some(pool) = harness::schema_pool("t_pg_stream_checkpoint").await else {
        return;
    };
    let store = PostgresStreamCheckpointStore::with_pool(pool.clone())
        .await
        .expect("checkpoint store");
    // Shared R1-R7 run on PostgreSQL too; reconnect additionally verifies the
    // exact acknowledged full frame, including retry_count and partial tools.
    let expected =
        awaken_store_conformance::stream_checkpoint::writes_are_exact(&store, "run-checkpoint")
            .await;
    let restarted = PostgresStreamCheckpointStore::with_pool(pool)
        .await
        .expect("restart");
    assert_eq!(
        restarted.get("run-checkpoint").await,
        Observed::Present(expected)
    );
    awaken_store_conformance::stream_checkpoint::delete_is_idempotent(&restarted, "run-checkpoint")
        .await;
}

/// R8 a stored row with undecodable checkpoint data is Unknown and remains
/// unchanged; R9 an unavailable pool is Unknown, not Absent; R10 writes/deletes
/// against that unavailable owner fail. These backend faults extend the shared
/// register laws without adding fault switches to the production port.
#[tokio::test]
async fn postgres_stream_checkpoint_corruption_and_unavailability_stay_unknown() {
    let Some(pool) = harness::schema_pool("t_pg_stream_checkpoint_faults").await else {
        return;
    };
    let store = PostgresStreamCheckpointStore::with_pool(pool.clone())
        .await
        .unwrap();
    let checkpoint = awaken_store_conformance::stream_checkpoint::sample("fault-run");
    store.put(checkpoint.clone()).await.unwrap();
    sqlx::query("UPDATE runtime_stream_checkpoint SET checkpoint='{}'::jsonb WHERE run_id=$1")
        .bind("fault-run")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        matches!(store.get("fault-run").await, Observed::Unknown(_)),
        "R8"
    );
    let stored: serde_json::Value =
        sqlx::query_scalar("SELECT checkpoint FROM runtime_stream_checkpoint WHERE run_id=$1")
            .bind("fault-run")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, serde_json::json!({}), "R8 retain damaged evidence");
    pool.close().await;
    assert!(
        matches!(store.get("missing").await, Observed::Unknown(_)),
        "R9"
    );
    assert!(store.put(checkpoint).await.is_err(), "R10 put");
    assert!(store.delete("fault-run").await.is_err(), "R10 delete");
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
        tokio::spawn(async move { a.claim("owner-a", 1_000, &Default::default()).await }),
        tokio::spawn(async move { b.claim("owner-b", 1_000, &Default::default()).await }),
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
    // Cause/effect graph: C1 owner A holds the database epoch guard after its
    // lease expires; C2 owner B attempts reclaim; C3 the guard is released.
    // Effects: E1 B cannot receive the row while C1 holds; E2 after C3, either
    // B's already-running attempt claims it or one retry does. Decision rules:
    // R1 C2 returns before C3 => it must be None, then retry; R2 C2 is still
    // pending at C3 => accept its post-release result or retry when it skipped.
    // The rule intentionally has no wall-clock threshold deciding correctness.
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
    AuthoritativeWallClock.advance_past(lease.expires_ms).await;

    let reclaim_store = store.clone();
    let mut reclaiming = tokio::spawn(async move {
        reclaim_store
            .claim("owner-b", 100, &Default::default())
            .await
    });
    // Claim intentionally uses SKIP LOCKED. Depending on scheduling it either
    // waits for the guard's transaction or immediately reports no claim; both
    // are safe, but it must never hand the guarded row to owner-b.
    let early = tokio::time::timeout(std::time::Duration::from_millis(50), &mut reclaiming).await;
    let early_result = match early {
        Ok(result) => {
            let result = result.unwrap().unwrap();
            assert!(
                result.is_none(),
                "the guarded row cannot be reclaimed before its commit returns"
            );
            Some(result)
        }
        Err(_) => None,
    };

    inner.release.notify_one();
    committing.await.unwrap().expect("commit");
    let first_after_release = match early_result {
        Some(result) => result,
        None => reclaiming.await.unwrap().unwrap(),
    };
    let reclaimed = match first_after_release {
        Some(reclaimed) => reclaimed,
        None => store
            .claim("owner-b", 100, &Default::default())
            .await
            .unwrap()
            .expect("reclaim after the guard releases"),
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
    let ingress = DispatchTestHarness::new(runtime, store.clone(), commit.clone(), system_clock());

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
        .claim("worker", 1_000, &Default::default())
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
            .claim("a", 0, &Default::default())
            .await
            .unwrap()
            .is_some()
    );
    let recovered = store.claim("b", 1_000, &Default::default()).await.unwrap();
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
    let ingress = DispatchTestHarness::new(runtime, store.clone(), commit.clone(), system_clock());

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
    harness::assert_message_idempotency_conflicts(
        &store,
        &awaken_dispatch_testkit::AuthoritativeWallClock,
    )
    .await;
}

#[tokio::test]
async fn scheduled_delivery_due_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_sched").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_scheduled_due(&store, &awaken_dispatch_testkit::AuthoritativeWallClock)
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
    harness::assert_millis_boundaries(&store, &awaken_dispatch_testkit::AuthoritativeWallClock)
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
    harness::assert_dead_letter(&store, &awaken_dispatch_testkit::AuthoritativeWallClock).await;
}

#[tokio::test]
async fn cancel_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_cancel").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_cancel(&store, &awaken_dispatch_testkit::AuthoritativeWallClock).await;
}

#[tokio::test]
async fn priority_dedupe_gc_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_pdg").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_priority_dedupe_gc(&store, &awaken_dispatch_testkit::AuthoritativeWallClock)
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
    harness::assert_lease_renewal(&store, &awaken_dispatch_testkit::AuthoritativeWallClock)
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
        store.claim("wa", 1_000, &capabilities),
        store.claim("wb", 1_000, &capabilities)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_candidate_pages_reach_work_after_incompatible_front_page() {
    // Causes: C1 140 higher-priority, same-order candidates need GPU; C2 the
    // registered requester lacks GPU; C3 a lower-priority compatible Run exists;
    // C4 compatible vs placed broad claim; C5 compatible Run absent/present;
    // C6 140 local candidates have an exhausted fencing epoch and one has the
    // largest still-claimable epoch; C7 140 fresh
    // peers share a Thread whose existing Run is still leased; C8 two eligible
    // successors sit behind the incompatible prefix while claimers race.
    // Effects: E1 scan past the first 128-row page without returning a false
    // None; E2 return only a compatible exact claim; E3 no compatible Run yields
    // None after exhausting every page; E4 a later enqueue is found on a fresh
    // call. Constraints: all rows have distinct Threads and the exact claim
    // transaction remains the only mutation. Decision table: R1=C1+C2+!C3 ->
    // E3; R2=C1+C2+C3+C4(compatible) -> E1+E2; R3=C1+C2+C3+C4(placed) ->
    // E1+E2; R4=R1 then C5(present) -> E4; R5=C6+one fresh local Run -> E1+E2;
    // R5a=C6+near-limit epoch -> the final valid epoch is claimed exactly;
    // R6=C7+one later independent Run -> E1+E2 without a second Thread writer;
    // R7=C1+C2+C8 -> two distinct exact claims, no false empty result after
    // one contender loses its first exact transaction.
    // The existing shared conformance owns
    // cancellation, reservation, recovery, priority and same-Thread partitions.
    let Some(pool) = harness::schema_pool("t_pg_candidate_pages").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    let (directory, identity) =
        awaken_dispatch_testkit::worker_http::ready_worker("page-worker").await;
    let worker = directory
        .current(&identity.worker_id)
        .await
        .unwrap()
        .unwrap()
        .snapshot;
    let mut gpu = PlacementRequirements::remote_required();
    gpu.required_capabilities.insert("gpu".into());
    let template = serde_json::to_value(
        RunDispatch::new(activation_on("template", "template-thread")).with_placement(gpu),
    )
    .unwrap();
    sqlx::query(
        "INSERT INTO runtime_dispatch (run_id, thread_id, request, status, priority) \
         SELECT 'page-incompatible-' || g, 'page-thread-' || g, \
                jsonb_set(jsonb_set($1::jsonb, '{activation,run_id}', \
                    to_jsonb('page-incompatible-' || g)), '{activation,thread_id}', \
                    to_jsonb('page-thread-' || g)), 'pending', 100 \
         FROM generate_series(1, 140) AS g",
    )
    .bind(sqlx::types::Json(&template))
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        store
            .claim_compatible(&worker, 60_000)
            .await
            .unwrap()
            .is_none(),
        "R1/E3"
    );

    for (index, placed) in [false, true].into_iter().enumerate() {
        let run = format!("page-compatible-{index}");
        let request = RunDispatch::new(activation_on(&run, &format!("compatible-thread-{index}")))
            .with_placement(PlacementRequirements::remote_required());
        store.enqueue(request).await.unwrap();
        let claimed = if placed {
            store
                .claim_placed(
                    &worker,
                    vec![worker.clone()],
                    Arc::new(LeastLoadedPolicy),
                    60_000,
                )
                .await
                .unwrap()
        } else {
            store.claim_compatible(&worker, 60_000).await.unwrap()
        }
        .expect("R2/R3/R4 later compatible Run is discovered");
        assert_eq!(claimed.lease.run_id.0, run, "R2/R3/E1+E2");
    }

    for index in 0..2 {
        store
            .enqueue(
                RunDispatch::new(activation_on(
                    &format!("page-raced-{index}"),
                    &format!("page-raced-thread-{index}"),
                ))
                .with_placement(PlacementRequirements::remote_required()),
            )
            .await
            .unwrap();
    }
    let (first, second) = tokio::join!(
        store.claim_compatible(&worker, 60_000),
        store.claim_compatible(&worker, 60_000),
    );
    let first = first.unwrap().expect("R7 first deep claim");
    let second = second.unwrap().expect("R7 second deep claim");
    assert_ne!(
        first.lease.run_id, second.lease.run_id,
        "R7 distinct winners"
    );
    assert!(first.lease.run_id.0.starts_with("page-raced-"), "R7");
    assert!(second.lease.run_id.0.starts_with("page-raced-"), "R7");

    harness::seed_exhausted_candidate_prefix(
        &pool,
        RunDispatch::new(activation_on("local-template", "local-template")),
        140,
    )
    .await;
    let near_limit = "page-near-limit";
    store
        .enqueue(RunDispatch::new(activation_on(
            near_limit,
            "page-near-limit-thread",
        )))
        .await
        .unwrap();
    sqlx::query(
        "UPDATE runtime_dispatch SET lease_epoch = 9223372036854775806, priority = 201 \
         WHERE run_id = $1",
    )
    .bind(near_limit)
    .execute(&pool)
    .await
    .unwrap();
    let local_run = "page-local-fresh";
    store
        .enqueue(RunDispatch::new(activation_on(
            local_run,
            "page-local-thread",
        )))
        .await
        .unwrap();
    let near_limit_claimed = store
        .claim("local-owner", 60_000, &Default::default())
        .await
        .unwrap()
        .expect("R5a final valid epoch remains discoverable");
    assert_eq!(near_limit_claimed.lease.run_id.0, near_limit, "R5a/E2");
    assert_eq!(near_limit_claimed.lease.epoch, i64::MAX as u64, "R5a");
    let local_claimed = store
        .claim("local-owner", 60_000, &Default::default())
        .await
        .unwrap()
        .expect("R5 scans past exhausted epoch candidates");
    assert_eq!(local_claimed.lease.run_id.0, local_run, "R5/E1+E2");

    let blocker = "page-thread-blocker";
    store
        .enqueue(RunDispatch::new(activation_on(blocker, "busy-thread")))
        .await
        .unwrap();
    let blocked = store
        .claim_run(
            &RunId(blocker.into()),
            "busy-owner",
            60_000,
            &Default::default(),
        )
        .await
        .unwrap()
        .expect("R6 predecessor holds the Thread");
    let blocked_template = serde_json::to_value(RunDispatch::new(activation_on(
        "blocked-template",
        "busy-thread",
    )))
    .unwrap();
    sqlx::query(
        "INSERT INTO runtime_dispatch (run_id, thread_id, request, status, priority) \
         SELECT 'page-blocked-' || g, 'busy-thread', \
                jsonb_set($1::jsonb, '{activation,run_id}', \
                    to_jsonb('page-blocked-' || g)), 'pending', 300 \
         FROM generate_series(1, 140) AS g",
    )
    .bind(sqlx::types::Json(&blocked_template))
    .execute(&pool)
    .await
    .unwrap();
    let independent = "page-independent";
    store
        .enqueue(RunDispatch::new(activation_on(
            independent,
            "independent-thread",
        )))
        .await
        .unwrap();
    let next = store
        .claim("local-owner", 60_000, &Default::default())
        .await
        .unwrap()
        .expect("R6 skips blocked Thread pages");
    assert_eq!(next.lease.run_id.0, independent, "R6/E1+E2");
    assert_eq!(
        blocked.lease.run_id.0, blocker,
        "R6 original owner retained"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_candidate_page_boundary_preserves_the_exact_tail_under_contention() {
    // Causes: C1 exhausted high-priority prefixes of 127, 128, or 129 rows;
    // C2 no lower-priority Run, then exactly one eligible Run; C3 one claimant
    // for the first two sizes and eight simultaneous claimants for the last.
    // Effects: E1 absence returns None; E2 the tail is found on both sides of
    // the 128-row boundary; E3 only one contender owns it and the remaining
    // calls return None. Rules B1=C1+!C2->E1, B2=C1+C2+!C3->E2,
    // B3=C1(129)+C2+C3->E2+E3. Every Run has its own Thread; the existing
    // exact transaction remains the sole mutation and fencing authority.
    for (count, schema) in [
        (127, "t_pg_candidate_boundary_127"),
        (128, "t_pg_candidate_boundary_128"),
        (129, "t_pg_candidate_boundary_129"),
    ] {
        let Some(pool) = harness::schema_pool(schema).await else {
            return;
        };
        let store = Arc::new(
            PostgresDispatchStore::with_pool(pool.clone())
                .await
                .unwrap(),
        );
        harness::seed_exhausted_candidate_prefix(
            &pool,
            RunDispatch::new(activation_on(
                "boundary-template",
                "boundary-template-thread",
            )),
            count,
        )
        .await;
        assert!(
            store
                .claim("boundary-owner", 60_000, &Default::default())
                .await
                .unwrap()
                .is_none(),
            "B1 no eligible Run after {count} exhausted rows"
        );
        let tail = format!("boundary-valid-{count}");
        store
            .enqueue(RunDispatch::new(activation_on(
                &tail,
                &format!("tail-thread-{count}"),
            )))
            .await
            .unwrap();
        if count < 129 {
            let claimed = store
                .claim("boundary-owner", 60_000, &Default::default())
                .await
                .unwrap()
                .expect("B2 tail remains discoverable");
            assert_eq!(claimed.lease.run_id.0, tail, "B2/E2 count={count}");
        } else {
            let barrier = Arc::new(tokio::sync::Barrier::new(9));
            let mut claimers = tokio::task::JoinSet::new();
            for index in 0..8 {
                let store = store.clone();
                let barrier = barrier.clone();
                claimers.spawn(async move {
                    barrier.wait().await;
                    store
                        .claim(
                            &format!("boundary-owner-{index}"),
                            60_000,
                            &Default::default(),
                        )
                        .await
                });
            }
            barrier.wait().await;
            let mut winners = Vec::new();
            while let Some(result) =
                tokio::time::timeout(std::time::Duration::from_secs(30), claimers.join_next())
                    .await
                    .expect("B3 every contender terminates")
            {
                if let Some(claimed) = result.unwrap().unwrap() {
                    winners.push(claimed.lease.run_id.0);
                }
            }
            assert_eq!(winners, vec![tail], "B3/E2+E3 one exact owner");
        }
        assert!(
            store
                .claim("boundary-final", 60_000, &Default::default())
                .await
                .unwrap()
                .is_none(),
            "E3 no duplicate successor at count={count}"
        );
    }
}

#[tokio::test]
async fn exhausted_epoch_deep_discovery_preserves_remote_audiences_and_expired_runs() {
    // Causes: C1 129 remote high-priority Pending rows have epoch MAX; C2 one
    // lower-priority remote Run has epoch MAX-1; C3 one fresh remote Run follows;
    // C4 the first winner later has an expired Running lease at epoch MAX.
    // Effects: E1 compatible discovery claims C2 at epoch MAX; E2 placed
    // discovery claims C3 at epoch one; E3 C4 cannot be reclaimed or make a
    // false successor. Rules A1=C1+C2->E1, A2=C1+C3->E2, A3=C1+C4->E3.
    // Shared conformance owns ordinary status transitions and placement policy.
    let Some(pool) = harness::schema_pool("t_pg_candidate_epoch_audiences").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .unwrap();
    let remote = PlacementRequirements::remote_required();
    harness::seed_exhausted_candidate_prefix(
        &pool,
        RunDispatch::new(activation_on(
            "boundary-template",
            "boundary-template-thread",
        ))
        .with_placement(remote.clone()),
        129,
    )
    .await;
    let (directory, identity) =
        awaken_dispatch_testkit::worker_http::ready_worker("boundary-remote-worker").await;
    let worker = directory
        .current(&identity.worker_id)
        .await
        .unwrap()
        .unwrap()
        .snapshot;
    let near_limit = "boundary-remote-near-limit";
    store
        .enqueue(
            RunDispatch::new(activation_on(near_limit, "boundary-remote-near-thread"))
                .with_placement(remote.clone()),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE runtime_dispatch SET lease_epoch = $1 WHERE run_id = $2")
        .bind(i64::MAX - 1)
        .bind(near_limit)
        .execute(&pool)
        .await
        .unwrap();
    let first = store
        .claim_compatible(&worker, 60_000)
        .await
        .unwrap()
        .expect("A1 last valid remote epoch");
    assert_eq!(first.lease.run_id.0, near_limit, "A1/E1");
    assert_eq!(first.lease.epoch, i64::MAX as u64, "A1/E1");
    sqlx::query("UPDATE runtime_dispatch SET lease_until = 0 WHERE run_id = $1")
        .bind(near_limit)
        .execute(&pool)
        .await
        .unwrap();
    let fresh = "boundary-remote-fresh";
    store
        .enqueue(
            RunDispatch::new(activation_on(fresh, "boundary-remote-fresh-thread"))
                .with_placement(remote),
        )
        .await
        .unwrap();
    let second = store
        .claim_placed(
            &worker,
            vec![worker.clone()],
            Arc::new(LeastLoadedPolicy),
            60_000,
        )
        .await
        .unwrap()
        .expect("A2 placed candidate remains discoverable");
    assert_eq!(second.lease.run_id.0, fresh, "A2/E2");
    assert_eq!(second.lease.epoch, 1, "A2/E2");
    assert!(
        store
            .claim_compatible(&worker, 60_000)
            .await
            .unwrap()
            .is_none(),
        "A3/E3 expired exhausted Run cannot be reclaimed"
    );
}

#[tokio::test]
async fn deep_discovery_releases_its_stream_before_one_connection_exact_claim() {
    // Causes: C1 the first 128 rows are local-incompatible; C2 one later local
    // row exists; C3 the store has exactly one PostgreSQL connection. Effects:
    // E1 the deep stream reaches C2; E2 it releases that connection before the
    // existing exact claim transaction; E3 the claim completes without a pool
    // self-deadlock. Decision table: S1=C1+C2+C3 -> E1+E2+E3. The ordinary
    // multi-connection and no-compatible partitions live in the page test.
    let schema = "t_pg_one_connection_deep";
    let Some(pool) = harness::schema_pool(schema).await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("migrated dispatch");
    let template = serde_json::to_value(
        RunDispatch::new(activation_on("remote-template", "remote-template-thread"))
            .with_placement(PlacementRequirements::remote_required()),
    )
    .unwrap();
    sqlx::query(
        "INSERT INTO runtime_dispatch (run_id, thread_id, request, status, priority) \
         SELECT 'remote-' || g, 'remote-thread-' || g, \
                jsonb_set(jsonb_set($1::jsonb, '{activation,run_id}', \
                    to_jsonb('remote-' || g)), '{activation,thread_id}', \
                    to_jsonb('remote-thread-' || g)), 'pending', 100 \
         FROM generate_series(1, 129) AS g",
    )
    .bind(sqlx::types::Json(&template))
    .execute(&pool)
    .await
    .unwrap();
    store
        .enqueue(RunDispatch::new(activation_on(
            "single-connection-local",
            "single-connection-thread",
        )))
        .await
        .unwrap();
    pool.close().await;
    let one = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(move |conn, _| {
            Box::pin(async move {
                conn.execute("SET search_path=t_pg_one_connection_deep")
                    .await?;
                Ok(())
            })
        })
        .connect(&harness::database_url())
        .await
        .unwrap();
    let one_store = PostgresDispatchStore::with_existing_pool(one)
        .await
        .expect("one-connection authority");
    let claimed = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        one_store.claim("one-owner", 60_000, &Default::default()),
    )
    .await
    .expect("S1/E3 no self-deadlock")
    .unwrap()
    .expect("S1/E1 compatible row");
    assert_eq!(claimed.lease.run_id.0, "single-connection-local", "S1/E2");
}

#[tokio::test]
async fn idle_thread_inbox_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_idle").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_idle_thread_inbox(&store, &awaken_dispatch_testkit::AuthoritativeWallClock)
        .await;
}

#[tokio::test]
async fn supersession_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_super").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_supersession(&store, &awaken_dispatch_testkit::AuthoritativeWallClock).await;
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
        &awaken_dispatch_testkit::AuthoritativeWallClock,
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
    harness::assert_dead_letter_ttl_gc(&store, &awaken_dispatch_testkit::AuthoritativeWallClock)
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
    harness::assert_relinquish_claim(&store, &awaken_dispatch_testkit::AuthoritativeWallClock)
        .await;
}

#[tokio::test]
async fn defer_claim_is_request_local_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_defer_claim").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_defer_claim_is_request_local(
        &store,
        &awaken_dispatch_testkit::AuthoritativeWallClock,
    )
    .await;
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
        &awaken_dispatch_testkit::AuthoritativeWallClock,
    )
    .await;
}

#[tokio::test]
async fn physical_attempt_admission_uses_the_postgres_authority_clock() {
    // Postgres clock-authority cause/effect decision table:
    // C1 the database minted a current claim; C2 the caller supplies a clock
    // value ahead of that lease; C3 the persisted owner/epoch still match.
    // P1 C1+C2+C3 => admit from the database clock and install the slot.
    // P2 !C1 or !C3 => the shared physical-attempt matrix fences the caller.
    // This test owns P1; the shared A1-A10 matrix above owns P2 and slot
    // occupancy. The extreme caller value makes a second clock authority fail
    // deterministically instead of depending on host/database scheduling skew.
    let Some(pool) = harness::schema_pool("t_pg_physical_attempt_clock").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool)
        .await
        .expect("dispatch");
    store
        .enqueue(RunDispatch::new(activation("physical-clock-authority-run")))
        .await
        .expect("enqueue");
    let claimed = store
        .claim("clock-owner", 5_000, &Default::default())
        .await
        .expect("claim query")
        .expect("claim");

    assert_eq!(
        store
            .begin_attempt(&RunClaim::from(&claimed.lease), None)
            .await
            .expect("physical admission"),
        awaken_dispatch::AttemptAdmission::Applied,
        "the Postgres aggregate must evaluate its DB lease on the DB clock"
    );
}

#[tokio::test]
async fn terminal_recovery_claim_rejects_caller_clock_skew() {
    // T1 a pending row + stale caller clock -> fresh DB-relative lease, not an
    // already-expired 1970 deadline. T2 a DB-live lease + future caller clock
    // -> no recovery claim, unchanged owner/epoch. Causes are independent of
    // external execution: this deliberately has no physical slot to mask a
    // wrong lease decision. A lease held by a live finisher is still exclusive.
    let Some(pool) = harness::schema_pool("t_pg_terminal_recovery_clock").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .unwrap();
    store
        .enqueue(RunDispatch::new(activation("terminal-clock")))
        .await
        .unwrap();
    let before: i64 =
        sqlx::query_scalar("SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::bigint")
            .fetch_one(&pool)
            .await
            .unwrap();
    let first = store
        .claim_for_terminal_recovery(&RunId("terminal-clock".into()), "first", 60_000)
        .await
        .unwrap()
        .unwrap();
    assert!(
        first.lease.expires_ms >= before as u64 + 60_000,
        "T1 new lease must be DB-relative, got {:?}",
        first.lease
    );
    assert!(
        store
            .claim_for_terminal_recovery(&first.lease.run_id, "skewed-successor", 60_000)
            .await
            .unwrap()
            .is_none(),
        "T2 caller clock cannot steal a DB-live lease"
    );
    assert!(
        store
            .claim_is_current(&RunClaim::from(&first.lease))
            .await
            .unwrap(),
        "T2 exact original authority preserved"
    );
}

#[tokio::test]
async fn retry_exhaustion_claim_rejects_caller_clock_skew() {
    // X1 ordinary DB claim is live, no physical slot, crash threshold zero;
    // X2 caller supplies far-future time. Effect: None, not an exhaustion claim
    // and not a loop that repeatedly selects a DB-ineligible row. The separate
    // threshold is zero intentionally: only lease eligibility may block this.
    let Some(pool) = harness::schema_pool("t_pg_retry_recovery_clock").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool).await.unwrap();
    store
        .enqueue(RunDispatch::new(activation("retry-clock")))
        .await
        .unwrap();
    let first = store
        .claim("first", 60_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        store.claim_retry_exhausted("skewed-successor", 60_000, 0),
    )
    .await
    .expect("X1/X2 selection must terminate")
    .unwrap();
    assert!(
        result.is_none(),
        "X1/X2 caller clock cannot exhaust a DB-live lease"
    );
    assert!(
        store
            .claim_is_current(&RunClaim::from(&first.lease))
            .await
            .unwrap(),
        "X1 unchanged original authority"
    );
}

#[tokio::test]
async fn exact_claim_mints_lease_after_waiting_for_thread_lock() {
    // L1 exact ordinary/terminal claim waits on a peer Thread transaction;
    // L2 time passes while pg_blocking_pids proves that precise wait. E1 after
    // the peer commits, the new deadline is based on a fresh DB clock, not on
    // time sampled before that wait. Matrix runs both entry modes through the
    // same lock+clock kernel; no live claim or execution is force-cancelled.
    let Some(pool) = harness::schema_pool("t_pg_exact_claim_lock_clock").await else {
        return;
    };
    let store = Arc::new(
        PostgresDispatchStore::with_pool(pool.clone())
            .await
            .unwrap(),
    );
    for terminal in [false, true] {
        let name = if terminal {
            "terminal-lock-clock"
        } else {
            "ordinary-lock-clock"
        };
        store
            .enqueue(RunDispatch::new(activation_on(name, name)))
            .await
            .unwrap();
        let mut blocker = pool.begin().await.unwrap();
        let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *blocker)
            .await
            .unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(name)
            .execute(&mut *blocker)
            .await
            .unwrap();
        let task = tokio::spawn({
            let store = store.clone();
            async move {
                if terminal {
                    store
                        .claim_for_terminal_recovery(&RunId(name.into()), "after-lock", 60_000)
                        .await
                } else {
                    store
                        .claim_run(
                            &RunId(name.into()),
                            "after-lock",
                            60_000,
                            &Default::default(),
                        )
                        .await
                }
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let blocked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid)))")
                    .bind(blocker_pid).fetch_one(&pool).await.unwrap();
                if blocked { break; }
                tokio::task::yield_now().await;
            }
        }).await.expect("L1 exact claim waits on this transaction");
        // The barrier above proves contention; this short real-clock advance
        // separates PostgreSQL millisecond samples without mocking its clock.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let released_at: i64 = sqlx::query_scalar(
            "SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::bigint",
        )
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
        blocker.commit().await.unwrap();
        let claimed = task.await.unwrap().unwrap().unwrap();
        assert!(
            claimed.lease.expires_ms >= released_at as u64 + 60_000,
            "E1 mode={terminal}: {:?}",
            claimed.lease
        );
    }
}

#[tokio::test]
async fn list_dispatches_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_list").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_list_dispatches(&store, &awaken_dispatch_testkit::AuthoritativeWallClock)
        .await;
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
    // C4 A's deliberately short lease then expires; C5 owner B reclaims through
    // a second coordinator with an independent production-sized recovery lease.
    // Effects: E1 B reads current Running truth and applies NeverReplay; E2 the
    // external tool runs once; E3 one terminal log survives A's fenced delayed
    // commit; E4 B remains live while it installs the authoritative snapshot and
    // reaches physical admission. Constraints: every claim installs an
    // authoritative snapshot before policy; only production executor return,
    // never abort/drop, clears the slot. The canonical production Worker lease
    // is also the fixture progress budget; it is not a correctness threshold:
    // renewal and claim fencing remain the authorities. The Memory end-to-end and
    // shared store conformance own the complementary C1+!C2 Blocked rule.
    // Decision rule: execute C1-C5 and assert E1-E4.
    const A_EXPIRY_LEASE: u64 = 1_000;
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
    let (runtime, ran, entered) = blocking_tool_runtime_with_entry_signal(release.clone());
    let run = RunId("run-1".to_string());
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .expect("enqueue");

    // Owner A claims explicitly, then drives in the background. Retaining the
    // exact lease lets the fixture observe the production return/renewal scope
    // without pretending a task abort is physical quiescence.
    let claimed_a = store
        .claim("owner-a", A_EXPIRY_LEASE, &Default::default())
        .await
        .expect("A claim query")
        .expect("A claims");
    let lease_a = claimed_a.lease.clone();
    let worker_a = Arc::new(
        DispatchWorker::new(runtime.clone(), store.clone(), commit_a.clone(), "owner-a")
            .with_lease_ms(A_EXPIRY_LEASE),
    );
    let a_handle = {
        let worker_a = worker_a.clone();
        tokio::spawn(async move { worker_a.drive_claimed(claimed_a, harness::clock(0)).await })
    };

    // The existing fixture's exact entry barrier proves A is blocked in this
    // tool; a counter or any unrelated committed lifecycle cannot substitute.
    tokio::time::timeout(
        std::time::Duration::from_millis(DEFAULT_LEASE_MS),
        entered.notified(),
    )
    .await
    .expect("A reached and blocked in the tool");

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
    let a_error =
        tokio::time::timeout(std::time::Duration::from_millis(DEFAULT_LEASE_MS), a_handle)
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
                .max(renewal_stopped_at_ms.saturating_add(A_EXPIRY_LEASE)),
        )
        .await;

    // Owner B's lease-expired reclaim recovers the committed Executing phase and
    // completes the run without entering the non-recoverable tool again.
    let worker_b = DispatchWorker::new(runtime, store.clone(), commit_b.clone(), "owner-b")
        .with_lease_ms(DEFAULT_LEASE_MS);
    let processed = worker_b
        .tick(harness::clock(A_EXPIRY_LEASE + 1))
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

#[tokio::test]
async fn discovery_skips_busy_thread_without_changing_exact_claim_or_epoch_authority() {
    // D1 first eligible Thread has an externally held advisory transaction;
    // D2 an independent eligible Thread exists or has already been claimed;
    // D3 the peer releases the lock. D1+D2 => discovery claims only the peer
    // before D1 releases; D1+!D2 => bounded None and unchanged Pending/epoch0;
    // D3 => original candidate becomes claimable with its own fresh epoch.
    // Exact ordinary/terminal waits and fresh DB-clock minting remain covered
    // by exact_claim_mints_lease_after_waiting_for_thread_lock above.
    let Some(pool) = harness::schema_pool("t_pg_discovery_thread_contention").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .unwrap();
    store
        .enqueue(RunDispatch::new(activation_on("a-busy", "thread-busy")))
        .await
        .unwrap();
    store
        .enqueue(RunDispatch::new(activation_on("b-free", "thread-free")))
        .await
        .unwrap();
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind("thread-busy")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let peer = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        store.claim("discover", 60_000, &Default::default()),
    )
    .await
    .expect("D1/D2 discovery must not wait for this held Thread")
    .unwrap()
    .unwrap();
    assert_eq!(peer.lease.run_id.0, "b-free", "D1/D2");
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            store.claim("discover-next", 60_000, &Default::default())
        )
        .await
        .expect("D1/!D2 busy-only discovery is bounded")
        .unwrap()
        .is_none()
    );
    let row: (String, i64) =
        sqlx::query_as("SELECT status,lease_epoch FROM runtime_dispatch WHERE run_id=$1")
            .bind("a-busy")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row, ("pending".into(), 0), "D1/!D2 no authority minted");
    blocker.rollback().await.unwrap();
    let released = store
        .claim("after-release", 60_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(released.lease.run_id.0, "a-busy", "D3");
    assert!(
        store
            .claim_is_current(&RunClaim::from(&released.lease))
            .await
            .unwrap()
    );
    assert!(
        store
            .claim_is_current(&RunClaim::from(&peer.lease))
            .await
            .unwrap()
    );
}
