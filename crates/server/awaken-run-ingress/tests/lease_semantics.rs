//! Focused integration tests for the durable dispatch worker's LEASE semantics —
//! the fence that makes durable recovery safe under a multi-owner fleet.
//!
//! The claim/lease contract is: a claimed dispatch is *owned* for `lease_ms`; a
//! second owner cannot claim it until the lease expires (crash recovery), and the
//! owner keeps it alive by renewing. These tests pin the five load-bearing rules:
//!
//! 1. a live lease fences a second claim (no two owners drive one run at once);
//! 2. an expired lease is reclaimed, then driven-and-settled exactly once;
//! 3. a *terminal committed record* short-circuits a stale reclaim — the worker
//!    settles Done without re-executing (committed truth is authority);
//! 4. lease renewal keeps a slow-but-alive owner from being stolen across many
//!    lease periods (the exact fence that stops a fleet double-drive); and
//! 5. the residual risk: a run reclaimed while its first execution is *genuinely
//!    still in flight* (owner slow, lease lapsed, renewal failed) IS re-executed.
//!    Test 5 demonstrates that double-execution and documents it as the gap that
//!    only lease renewal (test 4) fences — there is no fence token on commit/settle.

mod harness;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::store::run_store::RunStore;
use awaken_run_ingress::{
    DispatchOutcome, DispatchQueue, DispatchWorker, MemoryDispatchStore, RunExecutionRequest,
    SqliteDispatchStore,
};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime::memory::MemoryCommitCoordinator;

use harness::{activation, blocking_tool_runtime, counting_text_runtime, text_runtime};

const LEASE: u64 = 1_000;

// --- 1. A live lease fences a second claim ---------------------------------

/// Store-level spec: while owner A holds a live lease on the only runnable
/// dispatch, a second owner B claiming the same runnable set is handed nothing —
/// the run is not double-owned. Every backend must match, so the body lives once.
async fn assert_live_lease_fences_a_claim<S: DispatchQueue>(store: &S) {
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();

    // A claims at t=0 with a lease that expires at LEASE.
    let a = store.claim("owner-a", LEASE, 0).await.unwrap();
    assert_eq!(
        a.map(|c| c.lease.owner),
        Some("owner-a".to_string()),
        "the fresh run is claimable by A"
    );

    // B claiming mid-lease (t=LEASE/2) is fenced: the run is leased to A.
    assert!(
        store
            .claim("owner-b", LEASE, LEASE / 2)
            .await
            .unwrap()
            .is_none(),
        "a live lease fences a second owner's claim"
    );
    // Even at the instant of expiry (expires_ms == now is still held) B is fenced.
    assert!(
        store
            .claim("owner-b", LEASE, LEASE - 1)
            .await
            .unwrap()
            .is_none(),
        "the lease holds right up to its expiry"
    );
}

#[tokio::test]
async fn live_lease_fences_a_claim_memory() {
    assert_live_lease_fences_a_claim(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn live_lease_fences_a_claim_sqlite() {
    // The SQL claim predicate (`lease_until >= now`) must fence exactly as the
    // in-memory reference does.
    assert_live_lease_fences_a_claim(&SqliteDispatchStore::open_in_memory().expect("open")).await;
}

// --- 2. An expired lease is reclaimed, then driven+settled exactly once -----

#[tokio::test]
async fn expired_lease_is_reclaimed_and_driven_exactly_once() {
    // A claims D at t=0 and dies without settling (simulated: claim, never drive).
    // After the lease expires, B MUST reclaim, drive, and settle exactly once.
    let (runtime, infers) = counting_text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());

    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    assert!(
        store.claim("owner-a", LEASE, 0).await.unwrap().is_some(),
        "A claims the fresh run"
    );
    assert_eq!(infers.load(Ordering::SeqCst), 0, "A died before executing");

    // B's worker reclaims after expiry, drives to Done, and settles.
    let worker_b = DispatchWorker::new(runtime, store.clone(), commit.clone(), "owner-b")
        .with_lease_ms(LEASE);
    let processed = worker_b.tick(LEASE + 1).await.unwrap();
    assert_eq!(
        processed,
        Some((
            RunId("run-1".to_string()),
            Phase::Ended(EndCause::NaturalEnd)
        )),
        "B reclaims the expired lease and drives the run to completion"
    );
    assert_eq!(
        infers.load(Ordering::SeqCst),
        1,
        "the reclaimed run executed exactly once"
    );

    // The settled dispatch is gone and not claimable again.
    assert_eq!(store.dispatch_count(), 0, "D is settled");
    assert!(
        store
            .claim("owner-b", LEASE, LEASE + 2)
            .await
            .unwrap()
            .is_none(),
        "a settled dispatch is not claimable again"
    );
    assert_eq!(infers.load(Ordering::SeqCst), 1, "and never re-executed");
}

// --- 3. A terminal committed record short-circuits a stale reclaim ----------

#[tokio::test]
async fn stale_reclaim_of_a_completed_run_settles_without_re_executing() {
    // A claims D, drives it to a committed terminal record, then dies BEFORE it
    // settles the dispatch (the crash window between commit and settle). A stale
    // reclaim by B must settle Done from committed truth WITHOUT re-executing.
    let (runtime, infers) = counting_text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());

    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    // A takes the lease, then commits the run to a terminal Ended record by hand —
    // exactly what its worker would have committed — but never calls settle.
    assert!(store.claim("owner-a", LEASE, 0).await.unwrap().is_some());
    let ctx = RuntimeRunContext::new().with_commit(commit.clone());
    let phase = runtime.execute(activation("run-1"), ctx).await.unwrap();
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    assert_eq!(infers.load(Ordering::SeqCst), 1, "A executed the run once");
    let record = RunStore::get(commit.as_ref(), &RunId("run-1".to_string())).expect("record");
    assert_eq!(record.phase, Phase::Ended(EndCause::NaturalEnd));

    // B reclaims the still-Running dispatch (lease expired). The committed terminal
    // record is authority: the worker settles Done and does not re-run the model.
    let worker_b = DispatchWorker::new(runtime, store.clone(), commit.clone(), "owner-b")
        .with_lease_ms(LEASE);
    let processed = worker_b.tick(LEASE + 1).await.unwrap();
    assert_eq!(
        processed,
        Some((
            RunId("run-1".to_string()),
            Phase::Ended(EndCause::NaturalEnd)
        )),
        "B settles the completed run from committed truth"
    );
    assert_eq!(
        infers.load(Ordering::SeqCst),
        1,
        "the model ran exactly once — the terminal record fenced re-execution"
    );
    assert_eq!(store.dispatch_count(), 0, "the completed dispatch is removed");
}

// --- 4. Lease renewal keeps a slow-but-alive owner across many periods ------

#[tokio::test]
async fn renewal_keeps_a_slow_owner_across_multiple_lease_periods() {
    // owner-a holds a run and renews on a healthy heartbeat (the daemon's
    // `renew_owned_leases`). Across three lease periods, owner-b's recovery claim
    // is fenced every time; only once renewal STOPS does the lease expire and B
    // steal it. This is the fence that stops the fleet double-drive.
    let store = MemoryDispatchStore::new();
    let lease = 100u64;
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    assert!(
        store.claim("owner-a", lease, 0).await.unwrap().is_some(),
        "A claims at t=0 (expires 100)"
    );

    // Three heartbeats, each firing before the current lease expires (60<100,
    // 120<160, 180<220), each extending the lease a full period ahead.
    for (renew_at, poke_at) in [(60u64, 120u64), (120, 180), (180, 240)] {
        assert_eq!(
            store
                .renew_owned_leases("owner-a", lease, renew_at)
                .await
                .unwrap(),
            1,
            "the heartbeat renewed A's in-flight lease"
        );
        assert!(
            store.claim("owner-b", lease, poke_at).await.unwrap().is_none(),
            "a renewed lease is never reclaimable while A stays alive"
        );
    }

    // Renewal stops (A finally dies). The last renewal at t=180 expires at 280,
    // so at t=281 recovery hands the run to B.
    assert_eq!(
        store
            .claim("owner-b", lease, 281)
            .await
            .unwrap()
            .map(|c| c.lease.owner),
        Some("owner-b".to_string()),
        "once renewal stops, the expired lease is reclaimed"
    );
}

// --- 5. Mid-flight reclaim safety — THE risk --------------------------------

#[tokio::test]
async fn mid_flight_reclaim_re_executes_a_still_running_run() {
    // THE risk: a run reclaimed while its FIRST execution is genuinely still in
    // flight (owner slow, not dead; lease lapsed; renewal failed) is RE-EXECUTED.
    //
    // owner-a claims and starts driving; its tool blocks after A has committed a
    // `Running` fact (mid-step, no waiting ticket). Its lease lapses with no
    // renewal. owner-b reclaims — sees no ticket and a non-terminal `Running`
    // record, so it FALLS THROUGH and re-executes, running the tool a SECOND time.
    //
    // This is the exactly-once-side-effect gap: nothing on the commit/settle path
    // carries a fence token, so only a healthy lease renewal (test 4) prevents it.
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (runtime, ran) = blocking_tool_runtime(release.clone());
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());

    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();

    // Owner A drives in the background; it will block inside the tool after
    // committing the run's first `Running` step.
    let worker_a = Arc::new(
        DispatchWorker::new(runtime.clone(), store.clone(), commit.clone(), "owner-a")
            .with_lease_ms(LEASE),
    );
    let a_handle = {
        let worker_a = worker_a.clone();
        tokio::spawn(async move { worker_a.tick(0).await })
    };

    // Wait until A is frozen inside the tool (its first invocation).
    let frozen = tokio::time::timeout(Duration::from_secs(5), async {
        while ran.load(Ordering::SeqCst) < 1 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(frozen.is_ok(), "A reached and blocked in the tool");
    assert_eq!(ran.load(Ordering::SeqCst), 1, "the tool ran once (owner A)");

    // A is mid-flight: it committed a `Running` fact and parked NO waiting ticket.
    let record = RunStore::get(commit.as_ref(), &RunId("run-1".to_string())).expect("record");
    assert_eq!(
        record.phase,
        Phase::Running,
        "A committed a mid-flight Running fact"
    );
    assert!(
        commit.waiting_for(&RunId("run-1".to_string())).is_none(),
        "and there is no waiting ticket — the reclaim hits the no-ticket branch"
    );

    // B's lease-expired reclaim re-drives the SAME run to completion, running the
    // tool a SECOND time. This documents the double-execution gap.
    let worker_b = DispatchWorker::new(runtime, store.clone(), commit.clone(), "owner-b")
        .with_lease_ms(LEASE);
    let processed = worker_b.tick(LEASE + 1).await.unwrap();
    assert_eq!(
        processed,
        Some((
            RunId("run-1".to_string()),
            Phase::Ended(EndCause::NaturalEnd)
        )),
        "B reclaimed the still-running run and re-drove it to completion"
    );
    assert_eq!(
        ran.load(Ordering::SeqCst),
        2,
        "GAP: the tool executed TWICE — a slow-but-alive owner whose lease lapsed \
         was double-driven. Only lease renewal (test 4) fences this; there is no \
         fence token on commit/settle to reject the stale re-execution."
    );

    // Release A so the background task can unwind; its late settle is a no-op.
    release.add_permits(1);
    let _ = tokio::time::timeout(Duration::from_secs(5), a_handle).await;
}

// --- guard: text_runtime is a real end-to-end fresh run (sanity) ------------

#[tokio::test]
async fn fresh_claim_drives_a_run_to_completion() {
    // A minimal end-to-end sanity check that the worker under test drives a freshly
    // claimed run to a terminal phase on a single owner (no contention).
    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    let worker = DispatchWorker::new(runtime, store.clone(), commit, "solo").with_lease_ms(LEASE);
    let processed = worker.tick(0).await.unwrap();
    assert_eq!(
        processed,
        Some((
            RunId("run-1".to_string()),
            Phase::Ended(EndCause::NaturalEnd)
        ))
    );
    assert_eq!(store.dispatch_count(), 0);
    // Settling makes it non-claimable — the queue is now idle.
    assert!(worker.tick(1).await.unwrap().is_none());
    let _ = DispatchOutcome::Done;
}
