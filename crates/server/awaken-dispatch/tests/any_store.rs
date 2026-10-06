// @formal evidence worker.placement_extension_cannot_widen_eligibility
//! `AnyDispatchStore`, the runtime-selectable backend (ADR-0019 multi-node
//! wiring). Proves the enum-free wrapper delegates the full `Dispatch` bundle to
//! its active backend and preserves the owner-scoped claim/lease semantics the
//! fleet relies on. The SQLite paths always run; the Postgres path skips when no
//! database is reachable. Also exercises `DispatchTestHarness::with_owner`, the seam
//! that gives each fleet process its own unique claim owner.

mod harness;
#[path = "any_store/resolution_failure.rs"]
mod resolution_failure;

use awaken_dispatch_contract::ControlDispatchQueue;
use awaken_time::system_clock;
use std::sync::Arc;

use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::stream::checkpoint::StreamCheckpoint;
use awaken_dispatch::{
    AnyDispatchStore, DispatchCursor, DispatchOperation, DispatchOperationalFeed, DispatchOutcome,
    DispatchQueue, DispatchTestHarness, Inbox, LeastLoadedPolicy, MemoryDispatchStore,
    PlacementContext, PlacementError, PlacementPolicy, PlacementRequirements, RankedWorker,
    RunDispatch, SubmitOptions, WorkerIdentity, WorkerManifest, WorkerSnapshot, WorkerState,
};
use awaken_dispatch::{RunClaim, SettleOutcome, WorkerRecoveryMode};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_store_sqlite::SqliteCommitCoordinator;

use awaken_dispatch_testkit::ConformanceClock as _;
use harness::{TICKET, activation, activation_on, tool_runtime};

async fn any_in_memory() -> AnyDispatchStore {
    awaken_dispatch::open_sqlite_dispatch_store_in_memory(system_clock())
        .await
        .expect("open in-memory sqlite backend")
}

/// The same runtime-selected backend over a raw injected clock handle.
async fn any_in_memory_with_clock_handle(
    clock: std::sync::Arc<dyn awaken_time::WallClock>,
) -> AnyDispatchStore {
    awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock)
        .await
        .expect("open in-memory sqlite backend")
}

/// The same runtime-selected backend over a clock the test drives, because the
/// dispatch port now reads "now" from the store it was built with.
async fn any_in_memory_with_clock(
    clock: &awaken_dispatch_testkit::ManualStoreClock,
) -> AnyDispatchStore {
    awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clock())
        .await
        .expect("open in-memory sqlite backend")
}

#[tokio::test]
async fn execution_binding_is_frozen_with_the_physical_slot() {
    // Shared B1-B9 includes Any delegation, Memory and real SQLite transactions.
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    for store in [
        AnyDispatchStore::from_dispatch(Arc::new(MemoryDispatchStore::new(clock.clock()))),
        any_in_memory_with_clock(&clock).await,
    ] {
        harness::attempt_binding::assert_binding_lifecycle(&store, &clock).await;
        harness::attempt_binding::assert_closing_lifecycle(&store, &clock).await;
    }
}

#[tokio::test]
async fn returned_attempt_cannot_restart_under_the_same_claim() {
    // Causal graph: admitted A -> observed return/ACK -> empty slot -> delayed
    // duplicate begin(A). Reusing A would let a repeated ACK(A) clear a second
    // physical execution. R1 exact live admission retry before ACK is idempotent;
    // R2 same claim after ACK is fenced; R3 repeated ACK stays idempotent.
    // This is a prerequisite for accepting independently delayed stop evidence.
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    for store in [
        AnyDispatchStore::from_dispatch(Arc::new(MemoryDispatchStore::new(clock.clock()))),
        any_in_memory_with_clock(&clock).await,
    ] {
        harness::attempt_retirement::assert_retirement(&store, 0, &clock).await;
    }
}

#[tokio::test]
async fn non_postgres_wrapper_does_not_fabricate_a_checkpoint_authority() {
    // Cause/effect decision table: R1 Postgres -> paired durable checkpoint (covered
    // by `any_postgres_connect_and_claim`); R2 SQLite -> None so the host must bind
    // its durable FS checkpoint; R3 remote dispatch -> None so the claim-bound HTTP
    // path remains authoritative. This test owns R2/R3 and prevents an implicit
    // process-local checkpoint from returning inside the runtime-selected wrapper.
    assert!(
        any_in_memory().await.stream_checkpoint_store().is_none(),
        "R2"
    );
    let remote =
        AnyDispatchStore::from_dispatch(Arc::new(MemoryDispatchStore::new(system_clock())));
    assert!(remote.stream_checkpoint_store().is_none(), "R3");
}

#[tokio::test]
async fn compatible_claim_skips_ineligible_work_and_pins_the_incarnation() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = any_in_memory_with_clock(&clock).await;
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

    let mut manifest = worker_snapshot("worker", "boot-a", 7).manifest;
    manifest.capabilities.insert("cpu".to_string());
    let fingerprint = manifest.fingerprint().unwrap();
    let worker = WorkerSnapshot {
        identity: WorkerIdentity::new("worker", "boot-a", 7),
        state: WorkerState::Ready,
        manifest,
        capability_fingerprint: fingerprint.clone(),
        in_flight: 0,
        warm_environment_shapes: Default::default(),
        credential_observations: Default::default(),
        acp_capability_observations: Default::default(),
        expires_at_ms: 10_000,
    };
    clock.set(0);
    let claimed = store
        .claim_compatible(&worker, 1_000)
        .await
        .unwrap()
        .expect("the compatible lower-priority run is selected");
    assert_eq!(claimed.request.run_id().0, "cpu-run");
    assert_eq!(claimed.lease.owner, worker.identity.lease_owner());
    let assignment = claimed.assignment.expect("remote claim pins assignment");
    assert_eq!(assignment.identity, worker.identity);
    assert_eq!(assignment.capability_fingerprint, fingerprint);
}

async fn exact_backend_capability_claim_conformance(
    store: &dyn ControlDispatchQueue,
    clock: &dyn awaken_dispatch_testkit::ConformanceClock,
) {
    // Cause/effect graph:
    // immutable backend_ref -> exact required capability -> atomic claim
    // compatibility. Queue priority/order cannot let an incapable Worker take a
    // Run; the other backend remains pending for its matching Worker.
    //
    // | queued runs | Worker manifest | claim outcomes |
    // | codex + claude interleaved | acp:codex | every codex, zero claude |
    // | remaining claude | acp:claude | every claude |
    for index in 0..12 {
        let backend = if index % 2 == 0 {
            "acp:codex"
        } else {
            "acp:claude"
        };
        let mut placement = PlacementRequirements::remote_required();
        placement.required_capabilities.insert(backend.to_owned());
        store
            .enqueue(
                RunDispatch::new(activation_on(
                    &format!("{backend}-{index}"),
                    &format!("thread-{index}"),
                ))
                .with_placement(placement),
            )
            .await
            .unwrap();
    }

    let worker = |id: &str, backend: &str| {
        let mut manifest = worker_snapshot(id, "boot", 1).manifest;
        manifest.capabilities.clear();
        manifest.capabilities.insert(backend.to_owned());
        WorkerSnapshot {
            identity: WorkerIdentity::new(id, "boot", 1),
            capability_fingerprint: manifest.fingerprint().unwrap(),
            manifest,
            state: WorkerState::Ready,
            in_flight: 0,
            warm_environment_shapes: Default::default(),
            credential_observations: Default::default(),
            acp_capability_observations: Default::default(),
            expires_at_ms: 10_000,
        }
    };

    for (backend, expected) in [("acp:codex", 6), ("acp:claude", 6)] {
        let worker = worker(backend, backend);
        let mut claimed = 0;
        clock.set(0);
        while let Some(run) = store.claim_compatible(&worker, 100).await.unwrap() {
            assert!(
                run.request.run_id().0.starts_with(backend),
                "{backend} Worker claimed {}",
                run.request.run_id().0
            );
            store
                .settle(
                    run.request.run_id(),
                    run.lease.epoch,
                    DispatchOutcome::Done,
                    &[],
                )
                .await
                .unwrap();
            claimed += 1;
        }
        assert_eq!(claimed, expected, "{backend}");
    }
}

#[tokio::test]
async fn exact_backend_capability_routes_a_mixed_queue_on_every_local_store() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    exact_backend_capability_claim_conformance(&MemoryDispatchStore::new(clock.clock()), &clock)
        .await;
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    exact_backend_capability_claim_conformance(&any_in_memory_with_clock(&clock).await, &clock)
        .await;
}

fn worker_snapshot(id: &str, boot: &str, generation: u64) -> WorkerSnapshot {
    // Placement/recovery rows vary capability and incarnation, not protocol.
    // Explicit current support keeps those causes independently observable;
    // default/legacy denial is covered by the protocol admission matrix.
    let mut manifest = WorkerManifest::default();
    manifest.runtime_protocol.min = 1;
    manifest.runtime_protocol.max = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
    manifest.capabilities.insert("cpu".to_string());
    let capability_fingerprint = manifest.fingerprint().unwrap();
    WorkerSnapshot {
        identity: WorkerIdentity::new(id, boot, generation),
        state: WorkerState::Ready,
        manifest,
        capability_fingerprint,
        in_flight: 0,
        warm_environment_shapes: Default::default(),
        credential_observations: Default::default(),
        acp_capability_observations: Default::default(),
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

async fn policy_claim_conformance(
    store: &dyn ControlDispatchQueue,
    clock: &dyn awaken_dispatch_testkit::ConformanceClock,
) {
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

    clock.set(0);
    assert!(
        store
            .claim_placed(&busy, workers.clone(), Arc::new(LeastLoadedPolicy), 100,)
            .await
            .unwrap()
            .is_none(),
        "a worker rejected by the active preference cannot race into ownership"
    );
    clock.set(0);
    let claimed = store
        .claim_placed(&idle, workers, Arc::new(LeastLoadedPolicy), 100)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.assignment.unwrap().identity, idle.identity);
}

#[tokio::test]
async fn policy_claim_is_wired_through_memory_and_runtime_selected_sqlite() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    policy_claim_conformance(&MemoryDispatchStore::new(clock.clock()), &clock).await;
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    policy_claim_conformance(&any_in_memory_with_clock(&clock).await, &clock).await;
}

#[tokio::test]
async fn replacement_policy_cannot_override_never_replace() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = MemoryDispatchStore::new(clock.clock());
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
    clock.set(0);
    store
        .claim_placed(
            &original,
            vec![original.clone(), replacement.clone()],
            Arc::new(LeastLoadedPolicy),
            10,
        )
        .await
        .unwrap()
        .unwrap();
    clock.set(11);
    assert!(
        store
            .claim_placed(
                &replacement,
                vec![original.clone(), replacement.clone()],
                Arc::new(MostLoadedPolicy),
                10,
            )
            .await
            .unwrap()
            .is_none(),
        "extension ranking cannot widen durable recovery authority"
    );
    clock.set(11);
    assert!(
        store
            .claim_placed(
                &original,
                vec![original.clone()],
                Arc::new(MostLoadedPolicy),
                10,
            )
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn recovery_mode_is_enforced_from_the_durable_previous_assignment() {
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = any_in_memory_with_clock(&clock).await;
    let mut never = PlacementRequirements::remote_required();
    never.required_capabilities.insert("cpu".to_string());
    never.recovery = WorkerRecoveryMode::NeverReplace;
    store
        .enqueue(RunDispatch::new(activation_on("never", "never-thread")).with_placement(never))
        .await
        .unwrap();
    let original = worker_snapshot("worker", "boot-a", 1);
    let replacement = worker_snapshot("worker", "boot-b", 2);
    clock.set(0);
    let _first = store
        .claim_compatible(&original, 10)
        .await
        .unwrap()
        .unwrap();
    clock.set(11);
    assert!(
        store
            .claim_compatible(&replacement, 10)
            .await
            .unwrap()
            .is_none()
    );
    clock.set(11);
    assert!(
        store
            .claim_compatible(&original, 10)
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
    clock.set(20);
    let initial = store
        .claim_compatible(&original, 10)
        .await
        .unwrap()
        .unwrap();
    clock.set(31);
    assert!(
        store
            .claim_run_compatible(initial.request.run_id(), &replacement, 10)
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
    clock.set(31);
    assert!(
        store
            .claim_run_compatible(initial.request.run_id(), &replacement, 10)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn any_delegates_enqueue_claim_and_owner_scoped_lease() {
    // Causes: C1 the SQLite factory receives this caller-owned ManualStoreClock;
    // C2 now is before/after the live lease; C3 owner is original/foreign.
    // Effects: E1 one idempotent row and owner-a claim; E2 foreign owner cannot
    // claim before expiry; E3 foreign owner can reclaim after expiry. Rules:
    // F1 C1+C2(before)+C3(original) => E1; F2 C1+C2(before)+C3(foreign)
    // => E2; F3 C1+C2(after)+C3(foreign) => E3. The moved erasure delegates
    // to the same SQL row and does not install another clock or lease owner.
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = any_in_memory_with_clock(&clock).await;
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
    clock.set(0);
    let claimed = store
        .claim("owner-a", 1_000, &Default::default())
        .await
        .unwrap();
    assert_eq!(
        claimed.map(|c| c.lease.owner),
        Some("owner-a".to_string()),
        "claim records the claiming owner on the lease"
    );
    clock.set(500);
    assert!(
        store
            .claim("owner-b", 1_000, &Default::default())
            .await
            .unwrap()
            .is_none(),
        "a live lease blocks a second owner"
    );
    clock.set(1_001);
    let recovered = store
        .claim("owner-b", 1_000, &Default::default())
        .await
        .unwrap();
    assert_eq!(
        recovered.map(|c| c.lease.owner),
        Some("owner-b".to_string()),
        "an expired lease is reclaimed by the next owner"
    );
}

#[tokio::test]
async fn any_lets_two_owners_claim_distinct_runs() {
    // The multi-worker guarantee (ADR-0019): two distinct owners against one queue
    // claim two distinct runs, never the same one. Under single-writer-per-thread
    // (ADR-0022) "distinct runs" means distinct THREADS — two runs of one thread
    // cannot both be in flight (see `any_serializes_one_thread_across_workers`).
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = any_in_memory_with_clock(&clock).await;
    store
        .enqueue(RunDispatch::new(activation_on("run-1", "thread-1")))
        .await
        .unwrap();
    store
        .enqueue(RunDispatch::new(activation_on("run-2", "thread-2")))
        .await
        .unwrap();

    clock.set(0);
    let first = store
        .claim("worker-a", 1_000, &Default::default())
        .await
        .unwrap()
        .expect("one");
    clock.set(0);
    let second = store
        .claim("worker-b", 1_000, &Default::default())
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
    clock.set(0);
    assert!(
        store
            .claim("worker-c", 1_000, &Default::default())
            .await
            .unwrap()
            .is_none(),
        "no runnable dispatch remains once both are claimed"
    );
}

#[tokio::test]
async fn any_serializes_one_thread_across_workers() {
    // Single-writer-per-thread (ADR-0022): two runs of the SAME thread never run at
    // once. Enqueue run-2 while run-1 is already in flight (so run-2 is a fresh
    // pending run, not a supersession of run-1) — a second worker still cannot claim
    // it, and it becomes claimable only after run-1 settles.
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = any_in_memory_with_clock(&clock).await;
    store
        .enqueue(RunDispatch::new(activation_on("run-1", "thread-x")))
        .await
        .unwrap();
    clock.set(0);
    let first = store
        .claim("worker-a", 1_000, &Default::default())
        .await
        .unwrap()
        .expect("run-1 claims and is now in flight on thread-x");
    assert_eq!(first.request.run_id().0, "run-1");

    // A second run arrives on the same thread while run-1 runs.
    store
        .enqueue(RunDispatch::new(activation_on("run-2", "thread-x")))
        .await
        .unwrap();
    clock.set(1);
    assert!(
        store
            .claim("worker-b", 1_000, &Default::default())
            .await
            .unwrap()
            .is_none(),
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
    clock.set(2);
    let second = store
        .claim("worker-b", 1_000, &Default::default())
        .await
        .unwrap()
        .expect("run-2 claims after run-1 settled");
    assert_eq!(second.request.run_id().0, "run-2");
}

#[tokio::test]
async fn any_delegates_inbox_append_idempotency() {
    let store = any_in_memory().await;
    let input = harness::pending("msg-1", "run-1", TICKET, ResumeResult::allow());
    assert!(store.append(input.clone()).await.unwrap(), "first append");
    assert!(
        !store.append(input).await.unwrap(),
        "a duplicate message id is a no-op through the wrapper"
    );
}

#[tokio::test]
async fn any_exposes_the_builtin_backends_operational_feed() {
    // A1 global selection retains its fact; A2 exact Run retains the same
    // identity; A3 unknown Run returns empty with unchanged cursor. Dropping
    // the selector fails A3. The adjacent erased-adapter case owns A4.
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = any_in_memory_with_clock(&clock).await;
    store
        .enqueue(RunDispatch::new(activation("operations-run")))
        .await
        .expect("enqueue");
    clock.set(0);
    store
        .claim("operations-owner", 1_000, &Default::default())
        .await
        .expect("claim")
        .expect("runnable");

    let page = store
        .events_after(DispatchCursor(0), 10, None)
        .await
        .expect("built-in feed");

    assert!(matches!(
        page.events.as_slice(),
        [event]
            if matches!(
                &event.operation,
                DispatchOperation::Claimed { claim }
                    if claim.owner == "operations-owner"
            )
    ));
    let scoped = store
        .events_after(DispatchCursor(0), 1, Some(&RunId("operations-run".into())))
        .await
        .expect("A2 exact Run");
    assert_eq!(scoped.events, page.events, "A2 identity");
    let empty = store
        .events_after(
            DispatchCursor(0),
            1,
            Some(&RunId("operations-unknown".into())),
        )
        .await
        .expect("A3 unknown Run");
    assert!(empty.events.is_empty(), "A3");
    assert_eq!(empty.next_cursor, DispatchCursor(0), "A3");
}

#[tokio::test]
async fn any_fails_feed_reads_closed_for_a_dispatch_only_adapter() {
    // A4: no operational source is installed. Both selectors fail; unsupported
    // observation cannot become an empty successful read.
    let dispatch_only: Arc<dyn awaken_dispatch::Dispatch> =
        Arc::new(MemoryDispatchStore::new(system_clock()));
    let store = AnyDispatchStore::from_dispatch(dispatch_only);

    assert!(
        store
            .events_after(DispatchCursor(0), 1, None)
            .await
            .is_err(),
        "an erased legacy adapter cannot pretend to expose durable operations"
    );
    assert!(
        store
            .events_after(DispatchCursor(0), 1, Some(&RunId("run".into())))
            .await
            .is_err(),
        "A4 scoped reads cannot manufacture an absent feed"
    );
}

#[tokio::test]
async fn with_owner_drives_a_durable_run_over_any_sqlite() {
    // Cause/effect graph: C1=foreground durable submit; C2=Any wraps SQLite;
    // C3=the Worker binds a live claim verifier; C4=the Run reaches an external
    // tool boundary. Effects: E1=claim, renewal, verifier, and settlement retain
    // one edge-owned SystemClock source; E2=the first Step commits Awaiting;
    // E3=the tool has no side effect.
    // Decision rule R1=C1+C2+C3+C4 -> E1+E2+E3. A sentinel claim timestamp would
    // instead make the live verifier reject its own lease before the first Step.
    let (runtime, ran) = tool_runtime();
    let store = Arc::new(any_in_memory().await);
    let commit = Arc::new(
        SqliteCommitCoordinator::open_in_memory()
            .await
            .expect("commit"),
    );
    let ingress = DispatchTestHarness::with_owner(
        runtime,
        store.clone(),
        commit.clone(),
        "fleet-node-7",
        None,
        system_clock(),
    );

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
async fn postgres_factory_pairs_the_selected_store_with_its_wake() {
    // Causes: C1 a reachable isolated PostgreSQL schema; C2 the factory builds
    // its dispatch store and PG-NOTIFY wake from one supplied pool; C3 the
    // hint is published after claim. Effects: E1 the store claims the exact row
    // under the requested owner; E2 publish succeeds but cannot admit a foreign
    // claim. Rules: W1 !C1 => environmental skip; W2 C1+C2 => E1; W3 C1+C2+C3
    // => E2. The existing pg_notify_wake e2e owns cross-connection listener
    // delivery; this case owns factory pairing and dispatch authority.
    let Some(pool) = harness::schema_pool("t_any_factory_wake").await else {
        return;
    };
    let (store, wake) = awaken_dispatch::with_postgres_dispatch_pool_and_wake(
        pool,
        "t_any_factory_wake_channel",
    )
    .await
    .expect("W2 factory pairs the PostgreSQL store and wake");
    store
        .enqueue(RunDispatch::new(activation("wake-run")))
        .await
        .expect("W2 exact durable row");
    assert_eq!(
        store
            .claim("wake-owner", 30_000, &Default::default())
            .await
            .expect("W2 claim")
            .expect("W2 runnable row")
            .lease
            .owner,
        "wake-owner",
        "W2/E1"
    );
    wake.publish().await.expect("W3 paired wake transport");
    assert!(
        store
            .claim("foreign", 1_000, &Default::default())
            .await
            .expect("W3 exact queue still fenced")
            .is_none(),
        "W3/E2 hint cannot admit a foreign claim"
    );
}

#[tokio::test]
async fn any_postgres_connect_and_claim() {
    // Cause/effect graph: C1 Postgres selected; C2 schema reachable; C3 wrapper
    // recreated over the same schema. Effects: E1 dispatch delegates to Postgres;
    // E2 a paired checkpoint adapter is present; E3 checkpoint survives recreation.
    // Decision table: R1 C1+!C2 -> skip environmental acceptance; R2 C1+C2+!C3 ->
    // E1+E2; R3 C1+C2+C3 -> E1+E2+E3. SQLite/remote wrappers own the complementary
    // `None` checkpoint row and must use FS/claim-bound transport respectively.
    // Constraint/Invariant: AnyDispatchStore delegates to one selected backend
    // and never creates a parallel checkpoint authority. Decision rule: execute
    // R1-R3 according to environmental availability and recreation state.
    let schema = "t_any_store";
    let Some(_pool) = harness::schema_pool(schema).await else {
        return;
    };
    let store = awaken_dispatch::connect_postgres_dispatch_store(
        &harness::database_url_in_schema(schema),
        10,
    )
    .await
    .expect("connect postgres backend");
    store
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .expect("enqueue over postgres");
    let claimed = store
        .claim("pg-owner", 1_000, &Default::default())
        .await
        .expect("claim");
    assert_eq!(claimed.map(|c| c.lease.owner), Some("pg-owner".to_string()));

    let checkpoint = store
        .stream_checkpoint_store()
        .expect("Postgres wrapper carries its paired checkpoint store");
    let partial = StreamCheckpoint {
        run_id: "any-checkpoint".to_string(),
        thread_id: "any-thread".to_string(),
        model: "provider/model".to_string(),
        partial_text: "partial".to_string(),
        partial_tools: Vec::new(),
        retry_count: 0,
    };
    checkpoint
        .put(partial.clone())
        .await
        .expect("checkpoint put");

    let restarted = awaken_dispatch::connect_postgres_dispatch_store(
        &harness::database_url_in_schema(schema),
        10,
    )
    .await
    .expect("reconnect postgres backend");
    assert_eq!(
        restarted
            .stream_checkpoint_store()
            .expect("restarted wrapper carries checkpoint store")
            .get("any-checkpoint")
            .await,
        awaken_agent_contract::observation::Observed::Present(partial)
    );
}
