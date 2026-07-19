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

use awaken_agent_contract::agent::delegation::{
    DelegationGroup, DelegationId, DelegationKind, DelegationLimits, RequestDelegation,
};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::run_store::RunStore;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_run_ingress::{
    DelegationCas, DelegationStore, DispatchQueue, DispatchWorker, DurableRunIngress, Inbox,
    PendingInput, PostgresDispatchStore, RunExecutionRequest, SubmitOptions,
};
use awaken_runtime::RunIngress;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_store_postgres::PostgresCommitCoordinator;

use harness::{THREAD, TICKET, activation, activation_on, blocking_tool_runtime, tool_runtime};

#[tokio::test]
async fn postgres_delegation_group_is_durable_and_revision_fenced() {
    let schema = "t_pg_delegation_group";
    let Some(pool) = harness::schema_pool(schema).await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool).await.expect("store");
    let mut group = DelegationGroup::new(
        RunId("parent".into()),
        "coordinator",
        Vec::new(),
        0,
        DelegationLimits::new(3, 2, 4),
    );
    store.create(group.clone()).await.unwrap();
    let stale = store.load(&RunId("parent".into())).await.unwrap().unwrap();
    group
        .request(RequestDelegation {
            id: DelegationId("d1".into()),
            parent_call_id: "c1".into(),
            target_agent_id: "researcher".into(),
            child_run_id: RunId("child".into()),
            kind: DelegationKind::Remote,
        })
        .unwrap();
    assert_eq!(
        store.compare_and_set(0, group).await.unwrap(),
        DelegationCas::Applied { revision: 1 }
    );
    assert_eq!(
        store
            .compare_and_set(stale.revision, stale.group)
            .await
            .unwrap(),
        DelegationCas::Fenced {
            current_revision: 1
        }
    );
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
        PostgresDispatchStore::connect(&harness::database_url_in_schema(schema))
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
                RunExecutionRequest::new(activation_on(run, "cg-thread")),
                fresh.clone(),
            )
            .await
            .expect("enqueue");
    }

    // Race two claimers on separate tasks (separate pool connections).
    let a = store.clone();
    let b = store.clone();
    let (ra, rb) = tokio::join!(
        tokio::spawn(async move { a.claim("owner-a", 1_000, 0).await }),
        tokio::spawn(async move { b.claim("owner-b", 1_000, 0).await }),
    );
    let won = ra.unwrap().expect("claim a ok").is_some() as u8
        + rb.unwrap().expect("claim b ok").is_some() as u8;
    assert_eq!(
        won, 1,
        "exactly one of two concurrent same-thread claims wins (single-writer)"
    );
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
async fn durable_submit_awaits_then_delivered_decision_resumes_on_postgres() {
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
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());

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
                result: ResumeResult::Decision {
                    allow: true,
                    note: None,
                },
            },
            0,
        )
        .await
        .expect("resume");
    assert_eq!(resumed, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(ran.load(Ordering::SeqCst), 1);

    // Committed truth is terminal.
    let record = RunStore::get(&*commit, &RunId("run-1".to_string())).expect("run record");
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
            .enqueue(RunExecutionRequest::new(activation("run-1")))
            .await
            .expect("enqueue");
    }

    // A fresh store on the same database still has the accepted run to claim.
    let restarted = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch b");
    let claimed = restarted
        .claim("worker", 1_000, 0)
        .await
        .expect("claim")
        .expect("the enqueued run survived restart");
    assert_eq!(claimed.request.run_id().0, "run-1");
}

#[tokio::test]
async fn postgres_connect_applies_migrations_and_claim_recovers_a_lease() {
    // Create the isolated schema; connect() opens its own pool, so pin its
    // search_path via the URL.
    let schema = "t_pg_recover";
    if harness::schema_pool(schema).await.is_none() {
        return;
    }

    // connect() (not with_pool) applies the dispatch migrations on a fresh pool.
    let store = PostgresDispatchStore::connect(&harness::database_url_in_schema(schema))
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
}

#[tokio::test]
async fn postgres_append_is_idempotent_and_stale_input_is_dropped() {
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
    let ingress = DurableRunIngress::new(runtime, store.clone(), commit.clone());

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
        .deliver_resume(pending("stale", "old-ticket", true), 0)
        .await
        .expect("stale delivery");
    assert_eq!(state, RunState::Awaiting);
    assert_eq!(ran.load(Ordering::SeqCst), 0, "stale input did not resume");

    // The correctly-correlated input (the earlier "dup") now resumes the run.
    let state = ingress
        .deliver_resume(pending("good", TICKET, true), 0)
        .await
        .expect("good delivery");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(ran.load(Ordering::SeqCst), 1);
    let record = RunStore::get(&*commit, &RunId("run-1".to_string())).expect("record");
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
async fn scheduled_delivery_due_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_sched").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_scheduled_due(&store).await;
}

#[tokio::test]
async fn dead_letter_budget_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_dlq").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_dead_letter(&store).await;
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
    harness::assert_priority_dedupe_gc(&store).await;
}

#[tokio::test]
async fn lease_renewal_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_renew").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_lease_renewal(&store).await;
}

#[tokio::test]
async fn two_workers_claim_distinct_runs_on_postgres() {
    // The durable store is already a distributed queue: two concurrent claims
    // (FOR UPDATE SKIP LOCKED) take different runs, never the same one. The two runs
    // are on DISTINCT threads — the single-writer-per-thread invariant (ADR-0022)
    // makes at most one run per thread claimable at a time, so two claimable runs must
    // live on different threads for this concurrency property to be meaningful.
    let Some(pool) = harness::schema_pool("t_pg_multi").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    store
        .enqueue(RunExecutionRequest::new(activation_on("run-1", "thread-1")))
        .await
        .unwrap();
    store
        .enqueue(RunExecutionRequest::new(activation_on("run-2", "thread-2")))
        .await
        .unwrap();

    let (a, b) = tokio::join!(store.claim("wa", 1_000, 0), store.claim("wb", 1_000, 0));
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
    harness::assert_settle_fences_stale_epoch(&store).await;
}

#[tokio::test]
async fn current_epoch_tracks_the_fence_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_current_epoch").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_current_epoch_tracks_the_fence(&store).await;
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
    harness::assert_concurrent_recovery_yields_one_winner(store).await;
}

#[tokio::test]
async fn awaiting_settle_fences_stale_epoch_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_fence_await").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_awaiting_settle_fences_stale_epoch(&store).await;
}

#[tokio::test]
async fn dead_letter_ttl_gc_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_ttlgc").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_dead_letter_ttl_gc(&store).await;
}

#[tokio::test]
async fn renew_owned_leases_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_renewall").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_renew_owned_leases(&store).await;
}

#[tokio::test]
async fn renew_skips_far_from_expiry_on_postgres() {
    let Some(pool) = harness::schema_pool("t_pg_renewnear").await else {
        return;
    };
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .expect("dispatch");
    harness::assert_renew_skips_far_from_expiry(&store).await;
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

/// Postgres parity for the mid-flight reclaim exactly-once guarantee (memory-only
/// until now, `lease_semantics.rs`): a run reclaimed while its FIRST execution is
/// genuinely still in flight is RE-EXECUTED, but the Postgres commit coordinator's
/// terminal-is-final fence keeps the committed LOG exactly-once — the stale owner's
/// duplicate post-terminal commit is rejected, so the transcript never gains a
/// second terminal fact or a duplicate final assistant message. The worker absorbs
/// the rejected commit as an already-done settle. The external tool side effect
/// still runs twice (an at-least-once effect inherent to lease recovery).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_mid_flight_reclaim_keeps_the_committed_log_exactly_once() {
    const LEASE: u64 = 1_000;
    let schema = "t_pg_midflight";
    let Some(pool) = harness::schema_pool(schema).await else {
        return;
    };
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

    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (runtime, ran) = blocking_tool_runtime(release.clone());
    let run = RunId("run-1".to_string());
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .expect("enqueue");

    // Owner A drives in the background; it blocks inside the tool after committing the
    // run's first `Running` fact (mid-step, no awaiting ticket).
    let worker_a = Arc::new(
        DispatchWorker::new(runtime.clone(), store.clone(), commit.clone(), "owner-a")
            .with_lease_ms(LEASE),
    );
    let a_handle = {
        let worker_a = worker_a.clone();
        tokio::spawn(async move { worker_a.tick(0).await })
    };

    // Wait until A is frozen inside the tool.
    let frozen = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while ran.load(Ordering::SeqCst) < 1 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(frozen.is_ok(), "A reached and blocked in the tool");

    let record = RunStore::get(&*commit, &run).expect("A committed a record");
    assert_eq!(
        record.state,
        RunState::Running,
        "A committed a mid-flight Running"
    );

    // Owner B's lease-expired reclaim re-drives the same run to completion — running
    // the tool a SECOND time (the inherent double side effect).
    let worker_b =
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "owner-b").with_lease_ms(LEASE);
    let processed = worker_b.tick(LEASE + 1).await.expect("B drives");
    assert_eq!(
        processed,
        Some((run.clone(), RunState::Ended(EndCause::NaturalEnd))),
        "B reclaimed the still-running run and drove it to completion"
    );

    // Release A; its re-commit over the now-terminal run is FENCED (B already settled
    // Done under a higher epoch and removed the row). A's terminal settle applies
    // nothing, so A's tick resolves cleanly to `None` — it durably settled nothing and
    // abandons, rather than reporting a completion it did not own (the memory analogue
    // is `lease_semantics::mid_flight_reclaim_keeps_the_committed_log_exactly_once`).
    release.add_permits(1);
    let a_result = tokio::time::timeout(std::time::Duration::from_secs(10), a_handle)
        .await
        .expect("A joined")
        .expect("A did not panic")
        .expect("A resolved without a fatal error");
    assert_eq!(
        a_result, None,
        "the stale owner's re-drive is fenced: it settles nothing and abandons"
    );

    // Residual: the tool side effect ran twice (at-least-once).
    assert_eq!(ran.load(Ordering::SeqCst), 2, "the tool ran twice");

    // THE guarantee: exactly-once committed LOG. The committed transcript carries
    // exactly ONE final "all done" assistant message and the run's record is a single
    // terminal fact — the stale owner's duplicate terminal commit was fenced.
    let all_done = ThreadReader::committed_messages(&*commit, &ThreadId(THREAD.to_string()))
        .into_iter()
        .filter(|m| m.text_content().contains("all done"))
        .count();
    assert_eq!(
        all_done, 1,
        "exactly one final assistant message — no duplicate terminal turn"
    );
    let record = RunStore::get(&*commit, &run).expect("terminal record");
    assert_eq!(
        record.state,
        RunState::Ended(EndCause::NaturalEnd),
        "the run has a single terminal record"
    );
}
