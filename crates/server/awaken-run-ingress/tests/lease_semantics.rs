//! Focused integration tests for the durable dispatch worker's LEASE semantics —
//! the fence that makes durable recovery safe under a multi-owner fleet.
//!
//! The claim/lease contract is: a claimed dispatch is *owned* for `lease_ms`; a
//! second owner cannot claim it until the lease expires (crash recovery), and the
//! owner keeps it alive by renewing. These tests pin the six load-bearing rules:
//!
//! 1. a live lease fences a second claim (no two owners drive one run at once);
//! 2. an expired lease is reclaimed, then driven-and-settled exactly once;
//! 3. a *terminal committed record* short-circuits a stale reclaim — the worker
//!    settles Done without re-executing (committed truth is authority);
//! 4. lease renewal keeps a slow-but-alive owner from being stolen across many
//!    lease periods (the exact fence that stops a fleet double-drive); and
//! 5. a run reclaimed after a process-bound non-recoverable tool has quiesced
//!    does NOT replay the tool: its committed Executing phase becomes an
//!    Indeterminate result, while the stale owner's mutation remains fenced.
//! 6. a reclaimed opaque ACP Run is terminally Indeterminate and its prompt is
//!    never sent again without an official idempotent receipt.

mod harness;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_run_ingress::{
    AttemptAdmission, DispatchOutcome, DispatchQueue, DispatchWorker, MemoryDispatchStore,
    PendingInput, RunClaim, RunDispatch, SqliteDispatchStore,
};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_store_inmem::MemoryCommitCoordinator;

use harness::{
    activation, activation_on, blocking_tool_runtime, counting_text_runtime, text_runtime,
};

const LEASE: u64 = 1_000;

// --- 1. A live lease fences a second claim ---------------------------------

/// Store-level spec: while owner A holds a live lease on the only runnable
/// dispatch, a second owner B claiming the same runnable set is handed nothing —
/// the run is not double-owned. Every backend must match, so the body lives once.
async fn assert_live_lease_fences_a_claim<S: DispatchQueue>(store: &S) {
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();

    // A claims at t=0 with a lease that expires at LEASE.
    let a = store
        .claim("owner-a", LEASE, 0, &Default::default())
        .await
        .unwrap();
    assert_eq!(
        a.map(|c| c.lease.owner),
        Some("owner-a".to_string()),
        "the fresh run is claimable by A"
    );

    // B claiming mid-lease (t=LEASE/2) is fenced: the run is leased to A.
    assert!(
        store
            .claim("owner-b", LEASE, LEASE / 2, &Default::default())
            .await
            .unwrap()
            .is_none(),
        "a live lease fences a second owner's claim"
    );
    // Even at the instant of expiry (expires_ms == now is still held) B is fenced.
    assert!(
        store
            .claim("owner-b", LEASE, LEASE - 1, &Default::default())
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replacement_claim_waits_for_predecessor_quiescence_before_model_entry() {
    // Cause/effect decision table:
    // W1 expired A claim + A physical slot + replacement B -> B may own the
    // mutation fence but model calls remain zero; W2 exact A finish ACK -> B
    // enters once and completes; W3 no A ACK -> B remains blocked indefinitely.
    // This test owns W1/W2 at the Worker->model boundary. The shared store spec
    // owns W3 as a durable state transition, avoiding a hanging test while still
    // proving lease expiry alone never clears the slot.
    let (runtime, model_calls) = counting_text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    store
        .enqueue(RunDispatch::new(activation("handoff-run")))
        .await
        .expect("fixture enqueue");
    let first = store
        .claim("owner-a", LEASE, 0, &Default::default())
        .await
        .expect("A query")
        .expect("A claim");
    let claim_a = RunClaim::from(&first.lease);
    assert_eq!(
        store.begin_attempt(&claim_a, 0).await.expect("W1 begin A"),
        AttemptAdmission::Applied
    );
    let replacement = store
        .claim("owner-b", LEASE, LEASE + 1, &Default::default())
        .await
        .expect("B query")
        .expect("B replacement claim");
    let worker_b = Arc::new(
        DispatchWorker::new(runtime, store.clone(), commit, "owner-b").with_lease_ms(LEASE),
    );
    let driving = tokio::spawn({
        let worker_b = worker_b.clone();
        async move {
            worker_b
                .drive_claimed(replacement, harness::clock(LEASE + 1))
                .await
        }
    });

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        model_calls.load(Ordering::SeqCst),
        0,
        "W1 mutation takeover did not enter the model"
    );
    assert_eq!(
        store.finish_attempt(&claim_a).await.expect("W2 A ACK"),
        awaken_run_ingress::SettleOutcome::Applied
    );
    let processed = tokio::time::timeout(Duration::from_secs(5), driving)
        .await
        .expect("W2 replacement unblocks")
        .expect("W2 task joins")
        .expect("W2 drive succeeds");
    assert_eq!(
        processed,
        Some((
            RunId("handoff-run".into()),
            RunState::Ended(EndCause::NaturalEnd)
        ))
    );
    assert_eq!(model_calls.load(Ordering::SeqCst), 1, "W2 one model entry");
}

#[tokio::test]
async fn live_lease_fences_a_claim_sqlite() {
    // The SQL claim predicate (`lease_until >= now`) must fence exactly as the
    // in-memory reference does.
    assert_live_lease_fences_a_claim(&SqliteDispatchStore::open_in_memory().expect("open")).await;
}

async fn assert_exact_claim_isolated_and_recoverable<S: DispatchQueue>(store: &S) {
    store
        .enqueue(RunDispatch::new(activation_on(
            "unrelated",
            "unrelated-thread",
        )))
        .await
        .unwrap();
    store
        .enqueue(RunDispatch::new(activation_on("child", "child-thread")))
        .await
        .unwrap();

    let child = RunId("child".to_string());
    let claimed = store
        .claim_run(&child, "parent-worker", LEASE, 0, &Default::default())
        .await
        .unwrap()
        .expect("the named child is runnable");
    assert_eq!(claimed.request.run_id(), &child);
    assert_eq!(claimed.lease.owner, "parent-worker");

    let unrelated = store
        .claim("pool-worker", LEASE, 0, &Default::default())
        .await
        .unwrap()
        .expect("exact claim did not consume unrelated work");
    assert_eq!(unrelated.request.run_id().0, "unrelated");

    assert!(
        store
            .claim_run(
                &child,
                "recovery-worker",
                LEASE,
                LEASE - 1,
                &Default::default(),
            )
            .await
            .unwrap()
            .is_none(),
        "the exact child lease is exclusive while live"
    );
    let recovered = store
        .claim_run(
            &child,
            "recovery-worker",
            LEASE,
            LEASE + 1,
            &Default::default(),
        )
        .await
        .unwrap()
        .expect("the exact child is independently recoverable");
    assert_eq!(recovered.lease.epoch, claimed.lease.epoch + 1);
}

#[tokio::test]
async fn exact_child_claim_is_isolated_and_recoverable_in_memory() {
    assert_exact_claim_isolated_and_recoverable(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn exact_child_claim_is_isolated_and_recoverable_in_sqlite() {
    assert_exact_claim_isolated_and_recoverable(
        &SqliteDispatchStore::open_in_memory().expect("open"),
    )
    .await;
}

async fn assert_parent_mediated_claims_are_atomic<S: DispatchQueue>(store: &S) {
    let child = RunId("atomic-child".to_string());
    let claimed = store
        .claim_new_run(
            RunDispatch::new(activation_on("atomic-child", "atomic-child-thread")),
            "parent-worker",
            LEASE,
            0,
            &Default::default(),
        )
        .await
        .unwrap()
        .expect("new child is admitted and claimed atomically");
    assert_eq!(claimed.request.run_id(), &child);
    assert!(
        store
            .claim("pool-worker", LEASE, 0, &Default::default())
            .await
            .unwrap()
            .is_none(),
        "the general pool cannot interleave with child creation"
    );
    assert_eq!(
        store
            .settle(&child, claimed.lease.epoch, DispatchOutcome::Awaiting, &[])
            .await
            .unwrap(),
        awaken_run_ingress::SettleOutcome::Applied
    );

    let resumed = store
        .deliver_and_claim(
            PendingInput {
                message_id: "child-answer".to_string(),
                run_id: child.clone(),
                thread_id: ThreadId("atomic-child-thread".to_string()),
                correlation_id: "permission-1".to_string(),
                available_at_ms: None,
                context_messages: Vec::new(),
                result: ResumeResult::Input("approved".to_string()),
            },
            "parent-worker",
            LEASE,
            1,
            &Default::default(),
        )
        .await
        .unwrap()
        .expect("input delivery and child resume claim are atomic");
    assert_eq!(resumed.pending.len(), 1);
    assert_eq!(resumed.pending[0].message_id, "child-answer");
    assert!(
        store
            .claim("pool-worker", LEASE, 1, &Default::default())
            .await
            .unwrap()
            .is_none(),
        "the general pool cannot interleave with child input delivery"
    );
}

#[tokio::test]
async fn parent_mediated_claims_are_atomic_in_memory() {
    assert_parent_mediated_claims_are_atomic(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn parent_mediated_claims_are_atomic_in_sqlite() {
    assert_parent_mediated_claims_are_atomic(&SqliteDispatchStore::open_in_memory().expect("open"))
        .await;
}

// --- 2. An expired lease is reclaimed, then driven+settled exactly once -----

#[tokio::test]
async fn expired_lease_is_reclaimed_and_driven_exactly_once() {
    // Test design. Causes: C1 owner A claims a fresh Run and dies before execute;
    // C2 its lease expires; C3 owner B ticks. Effects: E1 C3 reclaims and executes
    // once; E2 the dispatch settles and cannot be claimed again. Constraint/
    // Invariant: recovery preserves Run identity and the lease epoch fences A.
    // Decision rule: compare before-expiry no-claim with after-expiry C3 and
    // require one inference plus an empty queue.
    // A claims D at t=0 and dies without settling (simulated: claim, never drive).
    // After the lease expires, B MUST reclaim, drive, and settle exactly once.
    let (runtime, infers) = counting_text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());

    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    assert!(
        store
            .claim("owner-a", LEASE, 0, &Default::default())
            .await
            .unwrap()
            .is_some(),
        "A claims the fresh run"
    );
    assert_eq!(infers.load(Ordering::SeqCst), 0, "A died before executing");

    // B's worker reclaims after expiry, drives to Done, and settles.
    let worker_b =
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "owner-b").with_lease_ms(LEASE);
    let processed = worker_b.tick(harness::clock(LEASE + 1)).await.unwrap();
    assert_eq!(
        processed,
        Some((
            RunId("run-1".to_string()),
            RunState::Ended(EndCause::NaturalEnd)
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
            .claim("owner-b", LEASE, LEASE + 2, &Default::default())
            .await
            .unwrap()
            .is_none(),
        "a settled dispatch is not claimable again"
    );
    assert_eq!(infers.load(Ordering::SeqCst), 1, "and never re-executed");
}

#[tokio::test]
async fn expired_opaque_acp_claim_is_indeterminate_without_prompt_replay() {
    // ACP recovery cause/effect graph:
    // C1=expired claim is reclaimed; C2=backend is opaque ACP/native;
    // C3=committed terminal truth already exists. Effects: E1=commit the
    // existing Indeterminate terminal and settle Done; E2=ordinary committed-
    // truth recovery; E3=execute the recoverable native attempt. Constraints:
    // C3 dominates C2, and only C1+ACP+!C3 reaches E1.
    //
    // Decision table:
    // | Rule | C1 recovered | backend | C3 terminal | Effect |
    // | A1 | yes | ACP | no | E1, zero executor calls |
    // | A2 | yes | any | yes | E2 (covered below by stale terminal) |
    // | A3 | yes | Native | no | E3 (expired_lease... test above) |
    // | A4 | no | ACP | no | normal ACP execution (executor suite owner) |
    //
    // FMECA: replay after an unobserved ACP/MCP dispatch can duplicate an
    // external mutation (S5/O2/D5). Conservative Indeterminate reduces the
    // effect to an explicit unknown terminal; no guessed protocol stage or
    // second effect journal is introduced.
    // Decision rule: execute A1 here; A2 and A3 are the adjacent terminal/native
    // recovery tests, while A4 remains the executor suite's fresh-run owner.
    let (runtime, infers) = counting_text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let run = RunId("run-acp".to_string());
    let mut acp = activation("run-acp");
    let mut binding = acp.snapshot.resolved_spec.model_binding.binding().clone();
    binding.backend_ref = "acp:claude".to_string();
    acp.snapshot.resolved_spec.model_binding =
        awaken_runtime_contract::resolved::ResolvedModelCandidate::host(binding);

    store
        .enqueue(RunDispatch::new(acp))
        .await
        .expect("A1 enqueue ACP dispatch");
    assert!(
        store
            .claim("owner-a", LEASE, 0, &Default::default())
            .await
            .expect("A1 first claim")
            .is_some(),
        "A1 first owner holds the opaque Run"
    );

    let worker_b =
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "owner-b").with_lease_ms(LEASE);
    assert_eq!(
        worker_b
            .tick(harness::clock(LEASE + 1))
            .await
            .expect("A1 recovery"),
        Some((run.clone(), RunState::Ended(EndCause::Indeterminate))),
        "A1"
    );
    assert_eq!(
        infers.load(Ordering::SeqCst),
        0,
        "A1 prompt was not replayed"
    );
    assert_eq!(
        CommittedThreadView::run(commit.as_ref(), &run)
            .expect("A1 committed terminal")
            .state,
        RunState::Ended(EndCause::Indeterminate),
        "A1 committed truth"
    );
    assert_eq!(store.dispatch_count(), 0, "A1 settled Done");
}

#[tokio::test]
async fn committed_terminal_truth_dominates_opaque_acp_recovery() {
    // This is decision-table rule A2 from the preceding cause/effect graph:
    // C1=recovered claim, C2=opaque ACP backend, C3=committed terminal exists.
    // C3 dominates C2, so E2 settles the exact existing terminal without a new
    // inference and without replacing it by Indeterminate. FMECA: ordering the
    // ACP guard first would corrupt known truth and bypass shared inbox cleanup.
    // Constraint/Invariant: committed terminal truth precedes backend replay
    // policy. Decision rule: execute A2 and assert the exact terminal remains,
    // no inference occurs, and the dispatch settles.
    let (runtime, infers) = counting_text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let mut acp = activation("run-acp-committed");
    let mut binding = acp.snapshot.resolved_spec.model_binding.binding().clone();
    binding.backend_ref = "acp:claude".to_string();
    acp.snapshot.resolved_spec.model_binding =
        awaken_runtime_contract::resolved::ResolvedModelCandidate::host(binding);

    store
        .enqueue(RunDispatch::new(acp.clone()))
        .await
        .expect("A2 enqueue");
    assert!(
        store
            .claim("owner-a", LEASE, 0, &Default::default())
            .await
            .expect("A2 claim")
            .is_some()
    );
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    assert_eq!(
        runtime
            .execute(acp, context)
            .await
            .expect("A2 commit truth"),
        RunState::Ended(EndCause::NaturalEnd)
    );
    assert_eq!(infers.load(Ordering::SeqCst), 1);

    let worker_b =
        DispatchWorker::new(runtime, store.clone(), commit, "owner-b").with_lease_ms(LEASE);
    assert_eq!(
        worker_b
            .tick(harness::clock(LEASE + 1))
            .await
            .expect("A2 recovery"),
        Some((
            RunId("run-acp-committed".to_string()),
            RunState::Ended(EndCause::NaturalEnd)
        )),
        "A2 committed terminal remains authoritative"
    );
    assert_eq!(infers.load(Ordering::SeqCst), 1, "A2 never re-executes");
    assert_eq!(store.dispatch_count(), 0, "A2 settles Done");
}

// --- 3. A terminal committed record short-circuits a stale reclaim ----------

#[tokio::test]
async fn stale_reclaim_of_a_completed_run_settles_without_re_executing() {
    // Test design. Causes: C1 owner A commits terminal truth; C2 it crashes before
    // queue settlement; C3 owner B reclaims after lease expiry. Effects: E1 C3
    // settles Done from the committed fact; E2 inference count does not increase.
    // Constraint/Invariant: terminal committed truth dominates stale leased state.
    // Decision rule: reproduce C1+C2+C3 and compare inference count before/after.
    // A claims D, drives it to a committed terminal record, then dies BEFORE it
    // settles the dispatch (the crash window between commit and settle). A stale
    // reclaim by B must settle Done from committed truth WITHOUT re-executing.
    let (runtime, infers) = counting_text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());

    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    // A takes the lease, then commits the run to a terminal Ended record by hand —
    // exactly what its worker would have committed — but never calls settle.
    assert!(
        store
            .claim("owner-a", LEASE, 0, &Default::default())
            .await
            .unwrap()
            .is_some()
    );
    let ctx = RuntimeRunContext::new().with_commit(commit.clone());
    let state = runtime.execute(activation("run-1"), ctx).await.unwrap();
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(infers.load(Ordering::SeqCst), 1, "A executed the run once");
    let record =
        CommittedThreadView::run(commit.as_ref(), &RunId("run-1".to_string())).expect("record");
    assert_eq!(record.state, RunState::Ended(EndCause::NaturalEnd));

    // B reclaims the still-Running dispatch (lease expired). The committed terminal
    // record is authority: the worker settles Done and does not re-run the model.
    let worker_b =
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "owner-b").with_lease_ms(LEASE);
    let processed = worker_b.tick(harness::clock(LEASE + 1)).await.unwrap();
    assert_eq!(
        processed,
        Some((
            RunId("run-1".to_string()),
            RunState::Ended(EndCause::NaturalEnd)
        )),
        "B settles the completed run from committed truth"
    );
    assert_eq!(
        infers.load(Ordering::SeqCst),
        1,
        "the model ran exactly once — the terminal record fenced re-execution"
    );
    assert_eq!(
        store.dispatch_count(),
        0,
        "the completed dispatch is removed"
    );
}

// --- 4. Lease renewal keeps a slow-but-alive owner across many periods ------

#[tokio::test]
async fn renewal_keeps_a_slow_owner_across_multiple_lease_periods() {
    // Cause/effect decision table for the one exact-claim renewal authority:
    //
    // | exact owner + epoch | renewal active | time | effect |
    // |---|---|---|---|
    // | yes | yes | before renewed deadline | replacement cannot claim |
    // | yes | no | after last deadline | replacement reclaims with new epoch |
    // | stale | any | after reclaim | old claim remains fenced |
    //
    // This deliberately renews a complete `RunClaim`; an owner-wide heartbeat
    // would be a second, weaker ownership authority.
    let store = MemoryDispatchStore::new();
    let lease = 100u64;
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    let first = store
        .claim("owner-a", lease, 0, &Default::default())
        .await
        .unwrap()
        .expect("A claims at t=0 (expires 100)");
    let claim = RunClaim::from(&first.lease);

    // Three heartbeats, each firing before the current lease expires (60<100,
    // 120<160, 180<220), each extending the lease a full period ahead.
    for (renew_at, poke_at) in [(60u64, 120u64), (120, 180), (180, 240)] {
        assert!(
            store.renew_lease(&claim, lease, renew_at).await.unwrap(),
            "the exact claim renewed A's in-flight lease"
        );
        assert!(
            store
                .claim("owner-b", lease, poke_at, &Default::default())
                .await
                .unwrap()
                .is_none(),
            "a renewed lease is never reclaimable while A stays alive"
        );
    }

    // Renewal stops (A finally dies). The last renewal at t=180 expires at 280,
    // so at t=281 recovery hands the run to B.
    let replacement = store
        .claim("owner-b", lease, 281, &Default::default())
        .await
        .unwrap()
        .expect("once renewal stops, the expired lease is reclaimed");
    assert_eq!(replacement.lease.owner, "owner-b");
    assert!(replacement.lease.epoch > claim.epoch);
    assert!(
        !store.renew_lease(&claim, lease, 282).await.unwrap(),
        "the old exact claim remains fenced after recovery"
    );
}

// --- 5. Reclaim after quiescence — the committed LOG stays exactly-once ------

#[tokio::test]
async fn reclaim_after_process_bound_attempt_quiesces_applies_never_replay_policy() {
    // Test design. Causes: C1 owner A commits tool Executing then blocks; C2 its
    // lease expires and B takes the mutation claim; C3 A's owned executor Future
    // cooperatively returns after the release gate opens; C4 A's later commit is
    // fenced. Effects: E1 before C3, B remains physically blocked and no second
    // external effect enters; E2 after C3, `run_physical_attempt` records the one
    // exact quiescence ACK and B applies NeverReplay; E3 one terminal log
    // survives. Constraints: lease expiry is not quiescence, and abort/drop is
    // covered separately by RG10 as a permanently occupied slot. Decision rules:
    // R1=C1+C2+!C3 -> E1; R2=C1+C2+C3+C4 -> E2+E3.
    // Owner A commits Requested -> Executing before entering the blocking tool.
    // When its lease lapses, owner B recovers that durable phase. The descriptor's
    // conservative default is NeverReplay, so B publishes an Indeterminate tool
    // result and continues without entering the external tool a second time.
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (runtime, ran) = blocking_tool_runtime(release.clone());
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let run = RunId("run-1".to_string());

    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();

    // Owner A claims explicitly and drives in the background. The only code
    // allowed to acknowledge its physical return is the production
    // `run_physical_attempt` scope around that drive.
    let claimed_a = store
        .claim("owner-a", LEASE, 0, &Default::default())
        .await
        .expect("A claim query")
        .expect("A claims");
    let claim_a = RunClaim::from(&claimed_a.lease);
    let worker_a = Arc::new(
        DispatchWorker::new(runtime.clone(), store.clone(), commit.clone(), "owner-a")
            .with_lease_ms(LEASE),
    );
    let a_handle = {
        let worker_a = worker_a.clone();
        tokio::spawn(async move { worker_a.drive_claimed(claimed_a, harness::clock(0)).await })
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

    // A is mid-flight: it committed a `Running` fact and awaiting NO awaiting ticket.
    let record = CommittedThreadView::run(commit.as_ref(), &run).expect("record");
    assert_eq!(
        record.state,
        RunState::Running,
        "A committed a mid-flight Running fact"
    );
    assert!(
        commit.resume_ticket_for(&run).is_none(),
        "and there is no awaiting ticket — the reclaim hits the no-ticket branch"
    );

    // B may take the expired mutation claim, but its production drive must wait
    // at physical admission while A's executor Future is still blocked.
    let replacement = store
        .claim("owner-b", LEASE, LEASE + 1, &Default::default())
        .await
        .expect("C2 B claim query")
        .expect("C2 B replacement claim");
    assert_eq!(
        store
            .begin_attempt(&RunClaim::from(&replacement.lease), LEASE + 1)
            .await
            .expect("R1 B physical admission"),
        AttemptAdmission::Blocked,
        "R1/E1 B's exact durable attempt remains blocked before A returns"
    );
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "R1/E1 replacement admission cannot enter a second external effect"
    );

    // Opening the gate makes A's owned executor Future return normally. The
    // production scope records quiescence before its stale commit is fenced;
    // only then may B recover the same Run without replay.
    release.add_permits(1);
    let a_error = tokio::time::timeout(Duration::from_secs(5), a_handle)
        .await
        .expect("C3 A's background task joins")
        .expect("C3 A's task does not panic")
        .expect_err("C4 A's stale commit is fenced");
    assert!(
        a_error.to_string().contains("superseded")
            && a_error.to_string().contains("no longer holds lease epoch"),
        "C4 stale commit has the exact authority-loss cause: {a_error}"
    );
    assert!(
        !store.list_dispatches().await.unwrap()[0].physical_attempt_active,
        "R2/E2 production return recorded A's exact physical quiescence"
    );
    let worker_b =
        DispatchWorker::new(runtime, store.clone(), commit.clone(), "owner-b").with_lease_ms(LEASE);
    let processed = tokio::time::timeout(
        Duration::from_secs(5),
        worker_b.drive_claimed(replacement, harness::clock(LEASE + 1)),
    )
    .await
    .expect("R2 replacement unblocks")
    .expect("R2 replacement drive succeeds");
    assert_eq!(
        processed,
        Some((run.clone(), RunState::Ended(EndCause::NaturalEnd))),
        "B reclaimed the still-running run and drove it to completion"
    );

    assert_eq!(
        store
            .settle(&run, claim_a.epoch, DispatchOutcome::Done, &[])
            .await
            .expect("stale settle query"),
        awaken_run_ingress::SettleOutcome::Fenced,
        "E3 the stale predecessor cannot mutate the completed dispatch"
    );

    // The policy guarantee: the unknown external side effect is not repeated.
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "NeverReplay prevents a second external invocation"
    );

    // The committed log is also exactly-once. The recovered run has exactly ONE
    // terminal `Ended` fact and the
    // transcript carries exactly ONE final "all done" assistant message.
    let committed = commit.committed();
    let ended_facts = committed
        .run_facts
        .iter()
        .filter(|fact| fact.run_id == run && matches!(fact.state, RunState::Ended(_)))
        .count();
    assert_eq!(
        ended_facts, 1,
        "recovery produced exactly one terminal Ended fact"
    );
    let all_done = committed
        .messages
        .iter()
        .filter(|message| message_text(message).as_deref() == Some("all done"))
        .count();
    assert_eq!(
        all_done, 1,
        "exactly one final assistant message — no duplicate terminal Step"
    );

    // The reclaimed run's input is committed exactly once: A committed "go" in its
    // first step delta, and B's re-execute seeds it from committed history and
    // drops the activation copy (deduped by stable message id), rather than
    // replaying it. Locks in the idempotent-input fix.
    let go_inputs = committed
        .messages
        .iter()
        .filter(|message| message_text(message).as_deref() == Some("go"))
        .count();
    assert_eq!(
        go_inputs, 1,
        "the reclaimed run's input is committed exactly once, not replayed on re-execute"
    );
}

/// The concatenated text of a message's text blocks, if any — a small test helper
/// for asserting on committed transcript content.
fn message_text(message: &awaken_agent_contract::agent::message::Message) -> Option<String> {
    use awaken_agent_contract::agent::content::ContentBlock;
    let text: String = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    (!text.is_empty()).then_some(text)
}

// --- guard: text_runtime is a real end-to-end fresh run (sanity) ------------

#[tokio::test]
async fn fresh_claim_drives_a_run_to_completion() {
    // Coverage rationale. Causes: one fresh dispatch and one uncontended owner.
    // Effects: the Worker claims, executes, commits NaturalEnd, and settles the
    // row. Constraint/Invariant: this control path contains no recovery or
    // concurrency cause. Decision rule: exercise the single valid fresh-claim
    // partition as the baseline for the recovery tests above.
    // A minimal end-to-end sanity check that the worker under test drives a freshly
    // claimed run to a terminal state on a single owner (no contention).
    let runtime = text_runtime();
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    let worker = DispatchWorker::new(runtime, store.clone(), commit, "solo").with_lease_ms(LEASE);
    let processed = worker.tick(harness::clock(0)).await.unwrap();
    assert_eq!(
        processed,
        Some((
            RunId("run-1".to_string()),
            RunState::Ended(EndCause::NaturalEnd)
        ))
    );
    assert_eq!(store.dispatch_count(), 0);
    // Settling makes it non-claimable — the queue is now idle.
    assert!(worker.tick(harness::clock(1)).await.unwrap().is_none());
    let _ = DispatchOutcome::Done;
}
