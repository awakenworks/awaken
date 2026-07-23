//! Reusable behavioural conformance for durable dispatch implementations.
//!
//! Backends and decorators call these functions from their own integration tests.
//! The suite intentionally depends only on the public contract: passing it proves a
//! wrapper preserves the worker-visible claim, fencing, and recovery semantics rather
//! than merely delegating a convenient subset of methods.

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_run_ingress_contract::RunDispatch;
use awaken_run_ingress_contract::dispatch::{
    DispatchOutcome, DispatchQueue, PendingInput, RunClaim, SettleOutcome, SubmitOptions,
};
use awaken_run_ingress_contract::operational::{
    DispatchCursor, DispatchOperation, DispatchOperationalFeed, LeaseLossReason,
};
use awaken_run_ingress_contract::{
    PlacementRequirements, WORKER_LOCAL_CREDENTIALS_CAPABILITY, WorkerCredentialRevision,
    WorkerIdentity, WorkerManifest, WorkerSnapshot, WorkerState,
};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};

const LEASE_MS: u64 = 1_000;

/// Capabilities which are deliberately absent from a database-less worker transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConformanceCapabilities {
    /// The adapter can hold a backend-local commit epoch guard.
    pub local_commit_guard: bool,
    /// The adapter persists and returns opaque sandbox bindings.
    pub sandbox_binding: bool,
    /// The adapter exposes the server-local durable completion projection.
    pub completion_events: bool,
}

/// Optional bridge for transports whose authoritative clock lives on the server.
/// Direct stores ignore it because their `now_ms` command argument is already the
/// clock input under test.
pub trait ConformanceClock: Send + Sync {
    fn set(&self, now_ms: u64);
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DirectCommandClock;

impl ConformanceClock for DirectCommandClock {
    fn set(&self, _now_ms: u64) {}
}

impl<F> ConformanceClock for F
where
    F: Fn(u64) + Send + Sync,
{
    fn set(&self, now_ms: u64) {
        self(now_ms);
    }
}

impl ConformanceCapabilities {
    /// Full store/decorator behaviour.
    pub const LOCAL_STORE: Self = Self {
        local_commit_guard: true,
        sandbox_binding: true,
        completion_events: true,
    };

    /// Worker-visible HTTP behaviour. Atomic commit is tested through the separate
    /// claimed-commit service; a worker cannot acquire a server-local row guard.
    pub const WORKER_TRANSPORT: Self = Self {
        local_commit_guard: false,
        sandbox_binding: false,
        completion_events: false,
    };
}

/// Run the shared dispatch suite against one fresh, isolated implementation.
///
/// `namespace` is incorporated into every id so a PostgreSQL schema can safely run
/// this alongside other suites. The caller must still provide a store with no live
/// rows using that namespace.
pub async fn assert_dispatch_conformance(
    store: &dyn DispatchQueue,
    namespace: &str,
    capabilities: ConformanceCapabilities,
) {
    assert_dispatch_conformance_with_clock(store, namespace, capabilities, &DirectCommandClock)
        .await;
}

pub async fn assert_dispatch_conformance_with_clock(
    store: &dyn DispatchQueue,
    namespace: &str,
    capabilities: ConformanceCapabilities,
    clock: &dyn ConformanceClock,
) {
    if capabilities.local_commit_guard {
        local_claims_skip_remote_only_work(store, namespace).await;
    }
    exact_claim_recovery_and_fencing(store, namespace, clock).await;
    parent_mediated_commands_are_atomic(store, namespace, clock).await;
    current_claim_guard_is_exact(store, namespace, capabilities, clock).await;
    sandbox_binding_survives_recovery(store, namespace, capabilities, clock).await;
    completion_is_atomic_and_prevents_resurrection(store, namespace, capabilities, clock).await;
}

/// Verify the durable operational feed independently from Run lifecycle truth.
///
/// The backend under test must be fresh enough that the supplied namespace does
/// not collide with live rows; pre-existing feed events are handled by taking an
/// initial cursor.
pub async fn assert_dispatch_operational_feed_conformance<S>(store: &S, namespace: &str)
where
    S: DispatchQueue + DispatchOperationalFeed + ?Sized,
{
    let baseline = store
        .events_after(DispatchCursor(0), usize::MAX)
        .await
        .expect("read operational baseline");
    let cursor = baseline.next_cursor;

    let settled_run = run_id(namespace, "operations-settled");
    store
        .enqueue(dispatch(
            namespace,
            "operations-settled",
            "operations-settled-thread",
        ))
        .await
        .expect("enqueue settled operational run");
    let first = store
        .claim_run(&settled_run, "operations-a", LEASE_MS, 0)
        .await
        .expect("first operational claim")
        .expect("settled run is runnable");
    let recovered = store
        .claim_run(&settled_run, "operations-b", LEASE_MS, LEASE_MS + 1)
        .await
        .expect("operational recovery")
        .expect("expired run is recoverable");
    assert_eq!(
        store
            .settle(&settled_run, first.lease.epoch, DispatchOutcome::Done, &[],)
            .await
            .expect("fenced settle verdict"),
        SettleOutcome::Fenced
    );
    assert_eq!(
        store
            .settle(
                &settled_run,
                recovered.lease.epoch,
                DispatchOutcome::Awaiting,
                &[],
            )
            .await
            .expect("current settle"),
        SettleOutcome::Applied
    );

    let dead_run = run_id(namespace, "operations-dead-letter");
    store
        .enqueue(dispatch(
            namespace,
            "operations-dead-letter",
            "operations-dead-letter-thread",
        ))
        .await
        .expect("enqueue dead-letter operational run");
    let dead_first = store
        .claim_run(&dead_run, "operations-c", LEASE_MS, 2_000)
        .await
        .expect("dead-letter first claim")
        .expect("dead-letter run is runnable");
    let dead_recovered = store
        .claim_run(
            &dead_run,
            "operations-d",
            LEASE_MS,
            dead_first.lease.expires_ms + 1,
        )
        .await
        .expect("dead-letter recovery")
        .expect("dead-letter run is recoverable");
    assert_eq!(
        store
            .reap(1, dead_recovered.lease.expires_ms + 1)
            .await
            .expect("reap exhausted run"),
        1
    );

    let cancelled_run = run_id(namespace, "operations-cancelled");
    store
        .enqueue(dispatch(
            namespace,
            "operations-cancelled",
            "operations-cancelled-thread",
        ))
        .await
        .expect("enqueue cancellation operational run");
    store
        .claim_run(&cancelled_run, "operations-e", LEASE_MS, 5_000)
        .await
        .expect("cancellation claim")
        .expect("cancellation run is runnable");
    assert_eq!(
        store
            .cancel(&cancelled_run)
            .await
            .expect("cancel leased run"),
        Some(thread_id(namespace, "operations-cancelled-thread"))
    );

    let page = store
        .events_after(cursor, 100)
        .await
        .expect("read operational transitions");
    assert!(
        page.events
            .windows(2)
            .all(|pair| pair[0].cursor < pair[1].cursor),
        "dispatch cursors are strictly increasing"
    );
    let operations = page
        .events
        .iter()
        .map(|event| &event.operation)
        .collect::<Vec<_>>();
    assert_eq!(operations.len(), 11, "only applied mutations emit facts");
    assert!(
        matches!(operations[0], DispatchOperation::Claimed { claim } if claim.run_id == settled_run)
    );
    assert!(matches!(
        operations[1],
        DispatchOperation::LeaseLost {
            claim,
            reason: LeaseLossReason::Expired
        } if claim.owner == "operations-a"
    ));
    assert!(matches!(
        operations[2],
        DispatchOperation::Reclaimed { previous, claim }
            if previous.owner == "operations-a" && claim.owner == "operations-b"
    ));
    assert!(matches!(
        operations[3],
        DispatchOperation::Settled {
            claim,
            outcome: DispatchOutcome::Awaiting
        } if claim.owner == "operations-b"
    ));
    assert!(
        matches!(operations[4], DispatchOperation::Claimed { claim } if claim.run_id == dead_run)
    );
    assert!(matches!(
        operations[5],
        DispatchOperation::LeaseLost {
            reason: LeaseLossReason::Expired,
            ..
        }
    ));
    assert!(matches!(operations[6], DispatchOperation::Reclaimed { .. }));
    assert!(matches!(
        operations[7],
        DispatchOperation::LeaseLost {
            reason: LeaseLossReason::RetryExhausted,
            ..
        }
    ));
    assert!(matches!(
        operations[8],
        DispatchOperation::DeadLettered {
            attempt_count: 1,
            ..
        }
    ));
    assert!(matches!(
        operations[9],
        DispatchOperation::Claimed { claim } if claim.run_id == cancelled_run
    ));
    assert!(matches!(
        operations[10],
        DispatchOperation::LeaseLost {
            claim,
            reason: LeaseLossReason::Cancelled
        } if claim.owner == "operations-e"
    ));

    let first_page = store
        .events_after(cursor, 2)
        .await
        .expect("first operational page");
    let second_page = store
        .events_after(first_page.next_cursor, 2)
        .await
        .expect("second operational page");
    assert_eq!(first_page.events.len(), 2);
    assert_eq!(second_page.events.len(), 2);
    assert_eq!(second_page.events[0], page.events[2]);
    let empty = store
        .events_after(page.next_cursor, 0)
        .await
        .expect("zero-sized operational page");
    assert!(empty.events.is_empty());
    assert_eq!(empty.next_cursor, page.next_cursor);
}

async fn local_claims_skip_remote_only_work(store: &dyn DispatchQueue, ns: &str) {
    let required_credential = WorkerCredentialRevision {
        source_id: format!("{ns}-worker-credential"),
        revision: 7,
    };
    let mut remote_placement = PlacementRequirements::remote_required();
    remote_placement
        .required_capabilities
        .insert(WORKER_LOCAL_CREDENTIALS_CAPABILITY.to_string());
    remote_placement
        .required_credentials
        .insert(required_credential.clone());

    let remote_id = run_id(ns, "worker-private");
    let local_id = run_id(ns, "local-fallback");
    store
        .enqueue_with(
            dispatch(ns, "worker-private", "worker-private-thread")
                .with_placement(remote_placement),
            SubmitOptions {
                priority: 100,
                ..SubmitOptions::default()
            },
        )
        .await
        .expect("enqueue worker-private run");
    store
        .enqueue(dispatch(ns, "local-fallback", "local-fallback-thread"))
        .await
        .expect("enqueue local fallback run");

    let local = store
        .claim("conformance-local", LEASE_MS, 0)
        .await
        .expect("local claim succeeds")
        .expect("local-compatible work is available");
    assert_eq!(
        local.request.run_id(),
        &local_id,
        "a local executor must skip higher-priority remote-only work"
    );
    assert_eq!(
        store
            .settle(&local_id, local.lease.epoch, DispatchOutcome::Done, &[])
            .await
            .expect("settle local fallback"),
        SettleOutcome::Applied
    );

    let mut manifest = WorkerManifest::default();
    manifest
        .capabilities
        .insert(WORKER_LOCAL_CREDENTIALS_CAPABILITY.to_string());
    let worker = WorkerSnapshot {
        identity: WorkerIdentity::new(format!("{ns}-worker"), "boot", 1),
        capability_fingerprint: manifest
            .fingerprint()
            .expect("worker-local manifest fingerprints"),
        manifest,
        state: WorkerState::Ready,
        in_flight: 0,
        available_credentials: [required_credential].into_iter().collect(),
        expires_at_ms: 10_000,
    };
    let remote = store
        .claim_compatible(&worker, LEASE_MS, 0)
        .await
        .expect("worker-compatible claim succeeds")
        .expect("the exact worker credential revision is available");
    assert_eq!(remote.request.run_id(), &remote_id);
    assert_eq!(
        store
            .settle(&remote_id, remote.lease.epoch, DispatchOutcome::Done, &[])
            .await
            .expect("settle worker-private run"),
        SettleOutcome::Applied
    );
}

async fn exact_claim_recovery_and_fencing(
    store: &dyn DispatchQueue,
    ns: &str,
    clock: &dyn ConformanceClock,
) {
    clock.set(0);
    let target = run_id(ns, "target");
    let unrelated = run_id(ns, "unrelated");
    store
        .enqueue(dispatch(ns, "unrelated", "unrelated-thread"))
        .await
        .expect("enqueue unrelated conformance run");
    store
        .enqueue(dispatch(ns, "target", "target-thread"))
        .await
        .expect("enqueue exact-claim target");
    if let Some(depth) = store.runnable_depth(0).await.expect("query runnable depth") {
        assert_eq!(depth, 2, "both freshly enqueued runs are claimable");
    }

    let first = store
        .claim_run(&target, "conformance-a", LEASE_MS, 0)
        .await
        .expect("exact claim succeeds")
        .expect("target is runnable");
    assert_eq!(first.request.run_id(), &target);
    assert_eq!(first.lease.owner, "conformance-a");
    assert!(!first.recovered, "a fresh exact claim is not recovery");
    assert!(first.lease.epoch > 0, "a claimed lease has a fence epoch");

    let other = store
        .claim("conformance-pool", LEASE_MS, 0)
        .await
        .expect("general claim succeeds")
        .expect("exact claim leaves unrelated work available");
    assert_eq!(other.request.run_id(), &unrelated);
    assert_eq!(
        store
            .settle(&unrelated, other.lease.epoch, DispatchOutcome::Done, &[],)
            .await
            .expect("settle unrelated"),
        SettleOutcome::Applied
    );

    clock.set(LEASE_MS);
    assert!(
        store
            .claim_run(&target, "conformance-b", LEASE_MS, LEASE_MS)
            .await
            .expect("live-boundary claim")
            .is_none(),
        "a lease remains live at its exact expiry boundary"
    );
    clock.set(LEASE_MS + 1);
    let recovered = store
        .claim_run(&target, "conformance-b", LEASE_MS, LEASE_MS + 1)
        .await
        .expect("recovery claim succeeds")
        .expect("expired target is recoverable");
    assert!(recovered.recovered, "an expired running lease is recovery");
    assert_eq!(recovered.lease.epoch, first.lease.epoch + 1);
    assert_eq!(
        store
            .settle(&target, first.lease.epoch, DispatchOutcome::Done, &[])
            .await
            .expect("stale settle returns a verdict"),
        SettleOutcome::Fenced,
        "the old epoch cannot settle a recovered run"
    );
    assert_eq!(
        store
            .settle(&target, recovered.lease.epoch, DispatchOutcome::Done, &[],)
            .await
            .expect("current settle"),
        SettleOutcome::Applied
    );
}

async fn parent_mediated_commands_are_atomic(
    store: &dyn DispatchQueue,
    ns: &str,
    clock: &dyn ConformanceClock,
) {
    clock.set(10_000);
    let child = run_id(ns, "child");
    let claimed = store
        .claim_new_run(
            dispatch(ns, "child", "child-thread"),
            "conformance-parent",
            LEASE_MS,
            10_000,
        )
        .await
        .expect("claim_new_run succeeds")
        .expect("new child is returned already claimed");
    assert_eq!(claimed.request.run_id(), &child);
    assert!(
        store
            .claim("conformance-pool", LEASE_MS, 10_000)
            .await
            .expect("pool claim after atomic admission")
            .is_none(),
        "the pool cannot interleave between child enqueue and exact claim"
    );
    assert_eq!(
        store
            .settle(&child, claimed.lease.epoch, DispatchOutcome::Awaiting, &[],)
            .await
            .expect("child enters awaiting"),
        SettleOutcome::Applied
    );

    clock.set(10_001);
    let message_id = format!("{ns}-child-answer");
    let resumed = store
        .deliver_and_claim(
            PendingInput {
                message_id: message_id.clone(),
                run_id: child.clone(),
                thread_id: thread_id(ns, "child-thread"),
                correlation_id: format!("{ns}-approval"),
                available_at_ms: None,
                result: ResumeResult::Input("approved".to_string()),
            },
            "conformance-parent",
            LEASE_MS,
            10_001,
        )
        .await
        .expect("deliver_and_claim succeeds")
        .expect("awaiting child is returned already claimed");
    assert_eq!(resumed.pending.len(), 1);
    assert_eq!(resumed.pending[0].message_id, message_id);
    assert!(
        store
            .claim("conformance-pool", LEASE_MS, 10_001)
            .await
            .expect("pool claim after atomic delivery")
            .is_none(),
        "the pool cannot interleave between input delivery and exact claim"
    );
    assert_eq!(
        store
            .settle(
                &child,
                resumed.lease.epoch,
                DispatchOutcome::Done,
                &[message_id],
            )
            .await
            .expect("finish child"),
        SettleOutcome::Applied
    );
}

async fn current_claim_guard_is_exact(
    store: &dyn DispatchQueue,
    ns: &str,
    capabilities: ConformanceCapabilities,
    clock: &dyn ConformanceClock,
) {
    if !capabilities.local_commit_guard {
        return;
    }
    clock.set(20_000);
    let run = run_id(ns, "guard");
    store
        .enqueue(dispatch(ns, "guard", "guard-thread"))
        .await
        .expect("enqueue guard run");
    let claimed = store
        .claim_run(&run, "conformance-guard", LEASE_MS, 20_000)
        .await
        .expect("claim guard run")
        .expect("guard run is runnable");
    let current = RunClaim::from(&claimed.lease);
    let stale = RunClaim {
        epoch: current.epoch.saturating_sub(1),
        ..current.clone()
    };
    assert!(
        store
            .lock_commit_epoch(&stale)
            .await
            .expect("stale guard lookup")
            .is_none(),
        "a stale epoch never acquires commit authority"
    );
    let guard = store
        .lock_commit_epoch(&current)
        .await
        .expect("current guard lookup")
        .expect("the exact current claim acquires authority");
    drop(guard);
    assert_eq!(
        store
            .settle(&run, current.epoch, DispatchOutcome::Done, &[])
            .await
            .expect("settle guarded run"),
        SettleOutcome::Applied
    );
}

async fn sandbox_binding_survives_recovery(
    store: &dyn DispatchQueue,
    ns: &str,
    capabilities: ConformanceCapabilities,
    clock: &dyn ConformanceClock,
) {
    if !capabilities.sandbox_binding {
        return;
    }
    clock.set(30_000);
    let run = run_id(ns, "sandbox");
    store
        .enqueue(dispatch(ns, "sandbox", "sandbox-thread"))
        .await
        .expect("enqueue sandbox run");
    let first = store
        .claim_run(&run, "conformance-sandbox-a", LEASE_MS, 30_000)
        .await
        .expect("claim sandbox run")
        .expect("sandbox run is runnable");
    let sandbox_ref = format!("opaque:{ns}");
    let bound = store
        .bind_sandbox(&RunClaim::from(&first.lease), &sandbox_ref)
        .await
        .expect("bind sandbox");
    assert_eq!(bound, SettleOutcome::Applied, "current claim binds sandbox");
    clock.set(first.lease.expires_ms + 1);
    let recovered = store
        .claim_run(
            &run,
            "conformance-sandbox-b",
            LEASE_MS,
            first.lease.expires_ms + 1,
        )
        .await
        .expect("recover sandbox run")
        .expect("expired sandbox run is recoverable");
    assert_eq!(recovered.sandbox.as_deref(), Some(sandbox_ref.as_str()));
    assert!(recovered.recovered);
    assert_eq!(
        store
            .settle(&run, recovered.lease.epoch, DispatchOutcome::Done, &[],)
            .await
            .expect("settle sandbox run"),
        SettleOutcome::Applied
    );
}

async fn completion_is_atomic_and_prevents_resurrection(
    store: &dyn DispatchQueue,
    ns: &str,
    capabilities: ConformanceCapabilities,
    clock: &dyn ConformanceClock,
) {
    if !capabilities.completion_events {
        return;
    }

    let baseline = store
        .completion_events_after(0, usize::MAX)
        .await
        .expect("read completion baseline");
    let cursor = baseline.last().map_or(0, |event| event.sequence);

    // Awaiting and a stale fenced Done are negative partitions: neither may
    // publish a completion fact.
    clock.set(40_000);
    let awaiting = run_id(ns, "completion-awaiting");
    store
        .enqueue(dispatch(
            ns,
            "completion-awaiting",
            "completion-awaiting-thread",
        ))
        .await
        .expect("enqueue awaiting control run");
    let awaiting_claim = store
        .claim_run(&awaiting, "conformance-completion", LEASE_MS, 40_000)
        .await
        .expect("claim awaiting control run")
        .expect("awaiting control run is runnable");
    assert_eq!(
        store
            .settle(
                &awaiting,
                awaiting_claim.lease.epoch,
                DispatchOutcome::Awaiting,
                &[],
            )
            .await
            .expect("settle awaiting control run"),
        SettleOutcome::Applied
    );
    assert!(
        store
            .completion_events_after(cursor, 1)
            .await
            .expect("query after awaiting settle")
            .is_empty(),
        "Awaiting does not emit a completion fact"
    );
    assert_eq!(
        store
            .cancel(&awaiting)
            .await
            .expect("remove awaiting control run"),
        Some(thread_id(ns, "completion-awaiting-thread"))
    );

    let request = dispatch(ns, "completion-done", "completion-done-thread");
    let done = request.run_id().clone();
    store
        .enqueue(request.clone())
        .await
        .expect("enqueue completion run");
    let claim = store
        .claim_run(&done, "conformance-completion", LEASE_MS, 40_000)
        .await
        .expect("claim completion run")
        .expect("completion run is runnable");
    assert_eq!(
        store
            .settle(
                &done,
                claim.lease.epoch.saturating_sub(1),
                DispatchOutcome::Done,
                &[],
            )
            .await
            .expect("stale completion settle"),
        SettleOutcome::Fenced
    );
    assert!(
        store
            .completion_events_after(cursor, 1)
            .await
            .expect("query after fenced settle")
            .is_empty(),
        "a fenced Done emits no completion fact"
    );

    assert_eq!(
        store
            .settle(&done, claim.lease.epoch, DispatchOutcome::Done, &[])
            .await
            .expect("apply completion settle"),
        SettleOutcome::Applied
    );
    let first_page = store
        .completion_events_after(cursor, 1)
        .await
        .expect("read first completion page");
    assert_eq!(first_page.len(), 1);
    assert_eq!(first_page[0].run_id, done);
    assert!(first_page[0].sequence > cursor);
    assert_eq!(
        store
            .completion_events_after(cursor, 1)
            .await
            .expect("replay first completion page"),
        first_page,
        "a consumer may replay the same cursor idempotently"
    );

    // Both admission commands consult the tombstone. A completed durable
    // identity can never become runnable or emit a second event.
    store
        .enqueue(request.clone())
        .await
        .expect("replayed enqueue is accepted as a no-op");
    assert!(
        store
            .claim_run(&done, "conformance-replay", LEASE_MS, 40_001)
            .await
            .expect("exact claim after replay")
            .is_none(),
        "completed run id does not resurrect through enqueue"
    );
    assert!(
        store
            .claim_new_run(request.clone(), "conformance-replay", LEASE_MS, 40_001)
            .await
            .expect("atomic admission after replay")
            .is_none(),
        "completed run id does not resurrect through claim_new_run"
    );
    let manifest = WorkerManifest::default();
    let worker = WorkerSnapshot {
        identity: WorkerIdentity::new("conformance-worker", "completion-boot", 1),
        state: WorkerState::Ready,
        capability_fingerprint: manifest
            .fingerprint()
            .expect("default worker manifest fingerprints"),
        manifest,
        in_flight: 0,
        available_credentials: Default::default(),
        expires_at_ms: 100_000,
    };
    assert!(
        store
            .claim_new_run_compatible(request, &worker, LEASE_MS, 40_001)
            .await
            .expect("compatible atomic admission after replay")
            .is_none(),
        "completed run id does not resurrect through compatible claim_new_run"
    );
    assert_eq!(
        store
            .completion_events_after(cursor, 2)
            .await
            .expect("read completion events after replay"),
        first_page,
        "replayed admission creates no duplicate completion"
    );
    assert!(
        store
            .completion_events_after(first_page[0].sequence, 1)
            .await
            .expect("advance completion cursor")
            .is_empty(),
        "an advanced cursor excludes the acknowledged event"
    );
    assert!(
        store
            .completion_events_after(cursor, 0)
            .await
            .expect("zero-sized completion page")
            .is_empty(),
        "a zero limit returns an empty page"
    );
}

fn dispatch(ns: &str, run: &str, thread: &str) -> RunDispatch {
    RunDispatch::new(RunActivation::new(
        run_id(ns, run),
        thread_id(ns, thread),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId(format!("{ns}-snapshot")),
            metadata: Default::default(),
            root_agent_id: AgentId(format!("{ns}-agent")),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint(format!("{ns}-fingerprint")),
                instructions: "conformance".to_string(),
                max_steps: 4,
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::new("provider", "model", "backend"),
                ),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint: CatalogFingerprint(format!("{ns}-fingerprint")),
        },
        vec![Message::text(
            MessageId(format!("{ns}-{run}-input")),
            Role::User,
            "run conformance",
        )],
    ))
}

fn run_id(ns: &str, suffix: &str) -> RunId {
    RunId(format!("{ns}-{suffix}"))
}

fn thread_id(ns: &str, suffix: &str) -> ThreadId {
    ThreadId(format!("{ns}-{suffix}"))
}
