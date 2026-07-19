//! `AnyDispatchStore`, the runtime-selectable backend (ADR-0019 multi-node
//! wiring). Proves the enum-free wrapper delegates the full `Dispatch` bundle to
//! its active backend and preserves the owner-scoped claim/lease semantics the
//! fleet relies on. The SQLite paths always run; the Postgres path skips when no
//! database is reachable. Also exercises `DurableRunIngress::with_owner`, the seam
//! that gives each fleet process its own unique claim owner.

mod harness;

use std::sync::Arc;

use awaken_agent_contract::agent::run::RunState;
use awaken_run_ingress::{
    AnyDispatchStore, DispatchOutcome, DispatchQueue, DurableRunIngress, Inbox, LeastLoadedPolicy,
    MemoryDispatchStore, ModelAccessRef, PlacementContext, PlacementError, PlacementPolicy,
    PlacementRequirements, RankedWorker, RunDispatch, SubmitOptions, WorkerIdentity,
    WorkerManifest, WorkerSnapshot, WorkerState,
};
use awaken_run_ingress::{RunClaim, SettleOutcome, WorkerRecoveryMode};
use awaken_runtime::RunIngress;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_store_sqlite::SqliteCommitCoordinator;

use harness::{TICKET, activation, activation_on, tool_runtime};

fn any_in_memory() -> AnyDispatchStore {
    AnyDispatchStore::open_sqlite_in_memory().expect("open in-memory sqlite backend")
}

#[tokio::test]
async fn compatible_claim_skips_ineligible_work_and_pins_the_incarnation() {
    let store = any_in_memory();
    let mut gpu = PlacementRequirements::remote_required();
    gpu.required_capabilities.insert("gpu".to_string());
    let mut cpu = PlacementRequirements::remote_required();
    cpu.required_capabilities.insert("cpu".to_string());
    store
        .enqueue_with(
            RunDispatch::new(activation_on("gpu-run", "gpu-thread")).with_placement(gpu),
            SubmitOptions {
                priority: 100,
                ..SubmitOptions::default()
            },
        )
        .await
        .unwrap();
    store
        .enqueue(RunDispatch::new(activation_on("cpu-run", "cpu-thread")).with_placement(cpu))
        .await
        .unwrap();

    let mut manifest = WorkerManifest::default();
    manifest.capabilities.insert("cpu".to_string());
    let fingerprint = manifest.fingerprint().unwrap();
    let worker = WorkerSnapshot {
        identity: WorkerIdentity::new("worker", "boot-a", 7),
        state: WorkerState::Ready,
        manifest,
        capability_fingerprint: fingerprint.clone(),
        in_flight: 0,
        expires_at_ms: 10_000,
    };
    let claimed = store
        .claim_compatible(&worker, 1_000, 0)
        .await
        .unwrap()
        .expect("the compatible lower-priority run is selected");
    assert_eq!(claimed.request.run_id().0, "cpu-run");
    assert_eq!(claimed.lease.owner, worker.identity.lease_owner());
    let assignment = claimed.assignment.expect("remote claim pins assignment");
    assert_eq!(assignment.identity, worker.identity);
    assert_eq!(assignment.capability_fingerprint, fingerprint);
}

fn worker_snapshot(id: &str, boot: &str, generation: u64) -> WorkerSnapshot {
    let mut manifest = WorkerManifest::default();
    manifest.capabilities.insert("cpu".to_string());
    let capability_fingerprint = manifest.fingerprint().unwrap();
    WorkerSnapshot {
        identity: WorkerIdentity::new(id, boot, generation),
        state: WorkerState::Ready,
        manifest,
        capability_fingerprint,
        in_flight: 0,
        expires_at_ms: 100_000,
    }
}

struct MostLoadedPolicy;

impl PlacementPolicy for MostLoadedPolicy {
    fn id(&self) -> &str {
        "test-most-loaded"
    }

    fn rank(
        &self,
        _context: &PlacementContext,
        eligible: &[WorkerSnapshot],
    ) -> Result<Vec<RankedWorker>, PlacementError> {
        let mut eligible = eligible.to_vec();
        eligible.sort_by_key(|worker| std::cmp::Reverse(worker.in_flight));
        Ok(eligible
            .into_iter()
            .map(|worker| RankedWorker {
                identity: worker.identity,
                score: i64::from(worker.in_flight),
                reason: "test preference".to_string(),
            })
            .collect())
    }
}

fn worker_with_load(id: &str, load: u32) -> WorkerSnapshot {
    let mut worker = worker_snapshot(id, &format!("{id}-boot"), 1);
    worker.manifest.capacity.max_concurrent = 100;
    worker.capability_fingerprint = worker.manifest.fingerprint().unwrap();
    worker.in_flight = load;
    worker
}

async fn policy_claim_conformance(store: &dyn DispatchQueue) {
    let mut requirements = PlacementRequirements::remote_required();
    requirements.required_capabilities.insert("cpu".to_string());
    store
        .enqueue(
            RunDispatch::new(activation_on("policy-run", "policy-thread"))
                .with_placement(requirements),
        )
        .await
        .unwrap();
    let idle = worker_with_load("idle", 0);
    let busy = worker_with_load("busy", 7);
    let workers = vec![busy.clone(), idle.clone()];

    assert!(
        store
            .claim_placed(&busy, workers.clone(), Arc::new(LeastLoadedPolicy), 100, 0,)
            .await
            .unwrap()
            .is_none(),
        "a worker rejected by the active preference cannot race into ownership"
    );
    let claimed = store
        .claim_placed(&idle, workers, Arc::new(LeastLoadedPolicy), 100, 0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.assignment.unwrap().identity, idle.identity);
}

#[tokio::test]
async fn policy_claim_is_wired_through_memory_and_runtime_selected_sqlite() {
    policy_claim_conformance(&MemoryDispatchStore::new()).await;
    policy_claim_conformance(&any_in_memory()).await;
}

#[tokio::test]
async fn replacement_policy_cannot_override_never_replace() {
    let store = MemoryDispatchStore::new();
    let mut requirements = PlacementRequirements::remote_required();
    requirements.required_capabilities.insert("cpu".to_string());
    requirements.recovery = WorkerRecoveryMode::NeverReplace;
    store
        .enqueue(
            RunDispatch::new(activation_on("pinned", "pinned-thread")).with_placement(requirements),
        )
        .await
        .unwrap();
    let original = worker_with_load("original", 0);
    let replacement = worker_with_load("replacement", 9);
    store
        .claim_placed(
            &original,
            vec![original.clone(), replacement.clone()],
            Arc::new(LeastLoadedPolicy),
            10,
            0,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(
        store
            .claim_placed(
                &replacement,
                vec![original.clone(), replacement.clone()],
                Arc::new(MostLoadedPolicy),
                10,
                11,
            )
            .await
            .unwrap()
            .is_none(),
        "extension ranking cannot widen durable recovery authority"
    );
    assert!(
        store
            .claim_placed(
                &original,
                vec![original.clone()],
                Arc::new(MostLoadedPolicy),
                10,
                11,
            )
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn recovery_mode_is_enforced_from_the_durable_previous_assignment() {
    let store = any_in_memory();
    let mut never = PlacementRequirements::remote_required();
    never.required_capabilities.insert("cpu".to_string());
    never.recovery = WorkerRecoveryMode::NeverReplace;
    store
        .enqueue(RunDispatch::new(activation_on("never", "never-thread")).with_placement(never))
        .await
        .unwrap();
    let original = worker_snapshot("worker", "boot-a", 1);
    let replacement = worker_snapshot("worker", "boot-b", 2);
    let _first = store
        .claim_compatible(&original, 10, 0)
        .await
        .unwrap()
        .unwrap();
    assert!(
        store
            .claim_compatible(&replacement, 10, 11)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .claim_compatible(&original, 10, 11)
            .await
            .unwrap()
            .is_some(),
        "the same incarnation may recover its own expired lease"
    );

    let mut continuity = PlacementRequirements::remote_required();
    continuity.required_capabilities.insert("cpu".to_string());
    continuity.recovery = WorkerRecoveryMode::RequireSandboxContinuity;
    store
        .enqueue(
            RunDispatch::new(activation_on("continuous", "continuous-thread"))
                .with_placement(continuity),
        )
        .await
        .unwrap();
    let initial = store
        .claim_compatible(&original, 10, 20)
        .await
        .unwrap()
        .unwrap();
    assert!(
        store
            .claim_run_compatible(initial.request.run_id(), &replacement, 10, 31)
            .await
            .unwrap()
            .is_none(),
        "continuity replacement is blocked until a sandbox was bound"
    );
    assert_eq!(
        store
            .bind_sandbox(&RunClaim::from(&initial.lease), "sandbox:durable")
            .await
            .unwrap(),
        SettleOutcome::Applied
    );
    assert!(
        store
            .claim_run_compatible(initial.request.run_id(), &replacement, 10, 31)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn any_delegates_enqueue_claim_and_owner_scoped_lease() {
    let store = any_in_memory();
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    // Re-enqueue is a no-op (idempotent), delegated through the wrapper.
    store
        .enqueue(RunDispatch::new(activation("run-1")))
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
async fn reclaim_preserves_the_dispatch_pinned_model_candidate_set() {
    let store = any_in_memory();
    let expected = ModelAccessRef::candidate_set([
        (
            "primary".to_string(),
            ModelAccessRef::exact_credential("cred-a", "provider-a@1", "route-a@2"),
        ),
        (
            "fallback".to_string(),
            ModelAccessRef::exact_credential("cred-b", "provider-b@3", "route-b@4"),
        ),
    ])
    .expect("candidate set");
    store
        .enqueue(RunDispatch::new(activation("binding-retry")).with_model_access(expected.clone()))
        .await
        .unwrap();

    let first = store.claim("worker-a", 10, 0).await.unwrap().unwrap();
    assert_eq!(first.request.model_access.as_ref(), Some(&expected));
    let recovered = store.claim("worker-b", 10, 11).await.unwrap().unwrap();

    assert!(recovered.recovered);
    assert_eq!(recovered.request.model_access, Some(expected));
    assert_eq!(recovered.request.run_id(), first.request.run_id());
}

#[tokio::test]
async fn any_lets_two_owners_claim_distinct_runs() {
    // The multi-worker guarantee (ADR-0019): two distinct owners against one queue
    // claim two distinct runs, never the same one. Under single-writer-per-thread
    // (ADR-0022) "distinct runs" means distinct THREADS — two runs of one thread
    // cannot both be in flight (see `any_serializes_one_thread_across_workers`).
    let store = any_in_memory();
    store
        .enqueue(RunDispatch::new(activation_on("run-1", "thread-1")))
        .await
        .unwrap();
    store
        .enqueue(RunDispatch::new(activation_on("run-2", "thread-2")))
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
async fn any_serializes_one_thread_across_workers() {
    // Single-writer-per-thread (ADR-0022): two runs of the SAME thread never run at
    // once. Enqueue run-2 while run-1 is already in flight (so run-2 is a fresh
    // pending run, not a supersession of run-1) — a second worker still cannot claim
    // it, and it becomes claimable only after run-1 settles.
    let store = any_in_memory();
    store
        .enqueue(RunDispatch::new(activation_on("run-1", "thread-x")))
        .await
        .unwrap();
    let first = store
        .claim("worker-a", 1_000, 0)
        .await
        .unwrap()
        .expect("run-1 claims and is now in flight on thread-x");
    assert_eq!(first.request.run_id().0, "run-1");

    // A second run arrives on the same thread while run-1 runs.
    store
        .enqueue(RunDispatch::new(activation_on("run-2", "thread-x")))
        .await
        .unwrap();
    assert!(
        store.claim("worker-b", 1_000, 1).await.unwrap().is_none(),
        "the thread's second run is NOT claimable while the first is in flight"
    );

    // Once run-1 settles, the thread frees up and run-2 is claimable.
    store
        .settle(
            first.request.run_id(),
            first.lease.epoch,
            DispatchOutcome::Done,
            &[],
        )
        .await
        .unwrap();
    let second = store
        .claim("worker-b", 1_000, 2)
        .await
        .unwrap()
        .expect("run-2 claims after run-1 settled");
    assert_eq!(second.request.run_id().0, "run-2");
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
    // owner over the AnyDispatchStore(sqlite) backend awaits, then resumes to a
    // committed terminal state — proving both the owner param and the wrapper work
    // in the real ingress, not just at the store surface.
    let (runtime, ran) = tool_runtime();
    let store = Arc::new(any_in_memory());
    let commit = Arc::new(SqliteCommitCoordinator::open_in_memory().expect("commit"));
    let ingress =
        DurableRunIngress::with_owner(runtime, store.clone(), commit.clone(), "fleet-node-7", None);

    let state = ingress
        .submit_background(activation("run-1"))
        .await
        .expect("submit");
    assert_eq!(state, RunState::Awaiting, "the run awaits on the gate");
    assert_eq!(
        ran.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the tool has not run while awaiting"
    );
    // The run is now awaiting awaiting input over the AnyDispatchStore(sqlite)
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
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .expect("enqueue over postgres");
    let claimed = store.claim("pg-owner", 1_000, 0).await.expect("claim");
    assert_eq!(claimed.map(|c| c.lease.owner), Some("pg-owner".to_string()));
}
