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
    CredentialRealizationReceipt, DispatchOutcome, DispatchQueue, PendingInput, RunClaim,
    SettleOutcome, SubmitOptions,
};
use awaken_run_ingress_contract::operational::{
    DispatchCursor, DispatchOperation, DispatchOperationalFeed, LeaseLossReason,
};
use awaken_run_ingress_contract::{
    LeastLoadedPolicy, PlacementRequirements, WORKER_LOCAL_CREDENTIALS_CAPABILITY,
    WorkerCredentialObservation, WorkerCredentialRevision, WorkerIdentity, WorkerManifest,
    WorkerSnapshot, WorkerState,
};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::{
    CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource,
    CredentialRealizationCapabilities, CredentialRealizationKind, CredentialRef, CredentialUsage,
    InferenceEndpoint, ModelExposurePolicy, PlaintextBoundary, PlaintextHolder,
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
    attempt_credentials_are_atomic_and_epoch_fenced(store, namespace, capabilities).await;
    incompatible_credentials_do_not_poison_broad_claims(store, namespace).await;
    parent_mediated_commands_are_atomic(store, namespace, clock).await;
    current_claim_guard_is_exact(store, namespace, capabilities, clock).await;
    sandbox_binding_survives_recovery(store, namespace, capabilities, clock).await;
    completion_is_atomic_and_prevents_resurrection(store, namespace, capabilities, clock).await;
}

/// Broad-claim credential admission cause graph:
///
/// C1 row is placement-compatible -> C2 credential attempt is admissible
///  ├─ F -> E1 broad selection skips this row and evaluates the next row
///  └─ T -> E2 broad selection claims it atomically.
/// An exact claim names one row, so C2 false remains an explicit error.
///
/// | Rule | Claim | First row C1 | First row C2 | Later valid row | Result |
/// |---|---|---|---|---|---|
/// | Q1 | broad | T | F | T | claim later valid row |
/// | Q2 | exact invalid | T | F | - | admission error |
/// | Q3 | policy broad | T | F | T | claim later valid row |
async fn incompatible_credentials_do_not_poison_broad_claims(store: &dyn DispatchQueue, ns: &str) {
    let holder = PlaintextHolder::new(
        PlaintextBoundary::Worker,
        awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
    );
    let incompatible_holder = PlaintextHolder::new(PlaintextBoundary::Worker, "other-worker");
    let mut invalid =
        credential_dispatch(ns, "poison-invalid", "poison-invalid", &incompatible_holder);
    invalid.placement = PlacementRequirements::remote_required();
    let mut valid = credential_dispatch(ns, "poison-valid", "poison-valid", &holder);
    valid.placement = PlacementRequirements::remote_required();
    store
        .enqueue_with(
            invalid.clone(),
            SubmitOptions {
                priority: 100,
                ..SubmitOptions::default()
            },
        )
        .await
        .expect("Q1 enqueue incompatible row");
    store
        .enqueue(valid.clone())
        .await
        .expect("Q1 enqueue valid row");
    let worker = credential_worker(ns, &holder, true);
    let claimed = store
        .claim_compatible(&worker, LEASE_MS, 50_000)
        .await
        .expect("Q1 broad claim")
        .expect("Q1 later valid row is claimable");
    assert_eq!(claimed.request.run_id(), valid.run_id(), "Q1");
    store
        .settle(
            &claimed.lease.run_id,
            claimed.lease.epoch,
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("Q1 settle valid row");
    assert!(
        store
            .claim_run_compatible(invalid.run_id(), &worker, LEASE_MS, 50_002)
            .await
            .is_err(),
        "Q2 exact claim preserves the admission failure"
    );
    let mut placed = credential_dispatch(ns, "poison-placed", "poison-placed", &holder);
    placed.placement = PlacementRequirements::remote_required();
    store
        .enqueue(placed.clone())
        .await
        .expect("Q3 enqueue valid row");
    let claimed = store
        .claim_placed(
            &worker,
            vec![worker.clone()],
            std::sync::Arc::new(LeastLoadedPolicy),
            LEASE_MS,
            50_003,
        )
        .await
        .expect("Q3 policy broad claim")
        .expect("Q3 later valid row is claimable");
    assert_eq!(claimed.request.run_id(), placed.run_id(), "Q3");
    store
        .settle(
            &claimed.lease.run_id,
            claimed.lease.epoch,
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("Q3 settle valid row");
}

/// Claim-credential cause-effect graph:
///
/// publication credential + exact holder + implemented backend cell + installed
/// capability -> one binding committed with the lease epoch. Missing capability
/// rejects without advancing the epoch; cancellation bypasses materialization and
/// stores no binding; recovery advances the epoch and replaces the whole binding
/// set. A receipt is accepted only for the current exact binding.
///
/// | Rule | credential | cancel | capability | claim | Result |
/// |---|---|---|---|---|---|
/// | A1 | none | F | - | fresh | empty binding set |
/// | A2 | exact | F | exact local evidence | fresh | epoch-1 binding |
/// | A3 | exact | F | missing local evidence | fresh | reject, no mutation |
/// | A4 | exact | F | exact registered manifest after reject | fresh | epoch remains 1 |
/// | A5 | exact | T | missing | fresh | claim, empty binding set |
/// | A6 | exact | F | exact | recovery | replacement binding at epoch+1 |
///
/// | Rule | claim | receipt | Result |
/// |---|---|---|---|
/// | R1 | current | exact | applied and exact replay applied |
/// | R2 | current | wrong mechanism | rejected |
/// | R3 | stale after recovery | formerly exact | fenced |
async fn attempt_credentials_are_atomic_and_epoch_fenced(
    store: &dyn DispatchQueue,
    ns: &str,
    conformance: ConformanceCapabilities,
) {
    let holder = PlaintextHolder::new(
        PlaintextBoundary::Worker,
        format!("{ns}.worker.credentials"),
    );
    let exact_worker = credential_worker(ns, &holder, true);
    let incapable_worker = credential_worker(ns, &holder, false);
    let exact_capabilities = CredentialRealizationCapabilities::from_manifest_capabilities(
        &exact_worker.manifest.capabilities,
    )
    .expect("exact Worker credential capabilities decode");
    let incapable_capabilities = CredentialRealizationCapabilities::from_manifest_capabilities(
        &incapable_worker.manifest.capabilities,
    )
    .expect("empty Worker credential capabilities decode");

    let local = credential_dispatch(ns, "credential-local", "credential-local-thread", &holder);
    let local_id = local.run_id().clone();
    store
        .enqueue(local)
        .await
        .expect("A2 enqueue local credential run");
    if conformance.local_commit_guard {
        assert!(
            store
                .claim_run(
                    &local_id,
                    "credential-incapable-local-owner",
                    LEASE_MS,
                    50_000,
                    &incapable_capabilities,
                )
                .await
                .is_err(),
            "A3 local claim cannot synthesize capability evidence from the request"
        );
    }
    let first = store
        .claim_run(
            &local_id,
            "credential-local-owner",
            LEASE_MS,
            50_000,
            &exact_capabilities,
        )
        .await
        .expect("A2 local claim succeeds")
        .expect("A2 credential run is runnable");
    assert_eq!(first.credential_bindings.len(), 1, "A2 one exact binding");
    assert_eq!(
        first.lease.epoch, 1,
        "A3 rejected admission did not mutate the durable lease epoch"
    );
    let first_binding = &first.credential_bindings[0];
    assert_eq!(first_binding.claim_epoch, first.lease.epoch);
    assert_eq!(
        first_binding.selected_realization_kind,
        CredentialRealizationKind::WorkerProviderAdapter
    );
    let receipt = CredentialRealizationReceipt::new(
        first_binding,
        CredentialRealizationKind::WorkerProviderAdapter,
    )
    .expect("R1 exact receipt builds");
    assert_eq!(
        store
            .record_credential_realization(&RunClaim::from(&first.lease), receipt.clone())
            .await
            .expect("R1 receipt persists"),
        SettleOutcome::Applied
    );
    assert_eq!(
        store
            .record_credential_realization(&RunClaim::from(&first.lease), receipt.clone())
            .await
            .expect("R1 exact retry is idempotent"),
        SettleOutcome::Applied
    );
    let mut wrong_mechanism = receipt.clone();
    wrong_mechanism.actual_realization_kind = CredentialRealizationKind::WorkerRelay;
    assert!(
        store
            .record_credential_realization(&RunClaim::from(&first.lease), wrong_mechanism)
            .await
            .is_err(),
        "R2 a mechanism mismatch is rejected"
    );

    let recovered = store
        .claim_run(
            &local_id,
            "credential-recovery-owner",
            LEASE_MS,
            first.lease.expires_ms + 1,
            &exact_capabilities,
        )
        .await
        .expect("A6 recovery succeeds")
        .expect("A6 expired credential run is recoverable");
    assert_eq!(recovered.lease.epoch, first.lease.epoch + 1);
    assert_eq!(recovered.credential_bindings.len(), 1);
    assert_eq!(
        recovered.credential_bindings[0].claim_epoch, recovered.lease.epoch,
        "A6 the binding set is replaced under the new claim epoch"
    );
    assert_eq!(
        store
            .record_credential_realization(&RunClaim::from(&first.lease), receipt)
            .await
            .expect("R3 stale receipt returns a fence verdict"),
        SettleOutcome::Fenced
    );
    store
        .settle(&local_id, recovered.lease.epoch, DispatchOutcome::Done, &[])
        .await
        .expect("settle recovered credential run");

    let remote = credential_dispatch(ns, "credential-remote", "credential-remote-thread", &holder)
        .with_placement(PlacementRequirements::remote_required());
    let remote_id = remote.run_id().clone();
    store
        .enqueue(remote)
        .await
        .expect("A3 enqueue remote credential run");
    assert!(
        store
            .claim_run_compatible(&remote_id, &incapable_worker, LEASE_MS, 60_000)
            .await
            .is_err(),
        "A3 immutable Worker capability evidence rejects the claim"
    );
    let admitted = store
        .claim_run_compatible(&remote_id, &exact_worker, LEASE_MS, 60_000)
        .await
        .expect("A4 exact-capability claim succeeds")
        .expect("A4 failed admission did not consume the runnable row");
    assert_eq!(
        admitted.lease.epoch, 1,
        "A4 failed admission did not advance the durable epoch"
    );
    assert_eq!(admitted.credential_bindings.len(), 1);
    store
        .settle(&remote_id, admitted.lease.epoch, DispatchOutcome::Done, &[])
        .await
        .expect("settle exact-capability run");

    let mut cancellation =
        credential_dispatch(ns, "credential-cancel", "credential-cancel-thread", &holder)
            .with_placement(PlacementRequirements::remote_required());
    cancellation.inference_plaintext_holder = None;
    let cancellation_id = cancellation.run_id().clone();
    store
        .enqueue(cancellation)
        .await
        .expect("A5 enqueue cancellation control run");
    store
        .cancel(&cancellation_id)
        .await
        .expect("A5 cancellation becomes durable");
    let cancellation_claim = store
        .claim_run_compatible(&cancellation_id, &incapable_worker, LEASE_MS, 70_000)
        .await
        .expect("A5 control claim bypasses credential admission")
        .expect("A5 cancellation is runnable");
    assert!(cancellation_claim.cancellation_requested);
    assert!(
        cancellation_claim.credential_bindings.is_empty(),
        "A5 terminal control never materializes a credential"
    );
    store
        .settle(
            &cancellation_id,
            cancellation_claim.lease.epoch,
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("settle cancellation control run");
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
        .claim_run(
            &settled_run,
            "operations-a",
            LEASE_MS,
            0,
            &Default::default(),
        )
        .await
        .expect("first operational claim")
        .expect("settled run is runnable");
    let recovered = store
        .claim_run(
            &settled_run,
            "operations-b",
            LEASE_MS,
            LEASE_MS + 1,
            &Default::default(),
        )
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
        .claim_run(
            &dead_run,
            "operations-c",
            LEASE_MS,
            2_000,
            &Default::default(),
        )
        .await
        .expect("dead-letter first claim")
        .expect("dead-letter run is runnable");
    let dead_recovered = store
        .claim_run(
            &dead_run,
            "operations-d",
            LEASE_MS,
            dead_first.lease.expires_ms + 1,
            &Default::default(),
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
        .claim_run(
            &cancelled_run,
            "operations-e",
            LEASE_MS,
            5_000,
            &Default::default(),
        )
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
        id: format!("{ns}-worker-credential"),
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
        .claim("conformance-local", LEASE_MS, 0, &Default::default())
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
        credential_observations: [WorkerCredentialObservation::available(
            required_credential,
            0,
            10_000,
        )]
        .into_iter()
        .collect(),
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
        .claim_run(&target, "conformance-a", LEASE_MS, 0, &Default::default())
        .await
        .expect("exact claim succeeds")
        .expect("target is runnable");
    assert_eq!(first.request.run_id(), &target);
    assert_eq!(first.lease.owner, "conformance-a");
    assert!(!first.recovered, "a fresh exact claim is not recovery");
    assert!(first.lease.epoch > 0, "a claimed lease has a fence epoch");
    assert!(
        first.credential_bindings.is_empty(),
        "A1 a credential-free publication carries no attempt binding"
    );

    let other = store
        .claim("conformance-pool", LEASE_MS, 0, &Default::default())
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
            .claim_run(
                &target,
                "conformance-b",
                LEASE_MS,
                LEASE_MS,
                &Default::default(),
            )
            .await
            .expect("live-boundary claim")
            .is_none(),
        "a lease remains live at its exact expiry boundary"
    );
    clock.set(LEASE_MS + 1);
    let recovered = store
        .claim_run(
            &target,
            "conformance-b",
            LEASE_MS,
            LEASE_MS + 1,
            &Default::default(),
        )
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
            &Default::default(),
        )
        .await
        .expect("claim_new_run succeeds")
        .expect("new child is returned already claimed");
    assert_eq!(claimed.request.run_id(), &child);
    assert!(
        store
            .claim("conformance-pool", LEASE_MS, 10_000, &Default::default(),)
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
            &Default::default(),
        )
        .await
        .expect("deliver_and_claim succeeds")
        .expect("awaiting child is returned already claimed");
    assert_eq!(resumed.pending.len(), 1);
    assert_eq!(resumed.pending[0].message_id, message_id);
    assert!(
        store
            .claim("conformance-pool", LEASE_MS, 10_001, &Default::default(),)
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
    let identity = WorkerIdentity::new(format!("{ns}-guard-worker"), "boot", 1);
    let claimed = store
        .claim_run(
            &run,
            &identity.lease_owner(),
            LEASE_MS,
            20_000,
            &Default::default(),
        )
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
    assert!(
        !store
            .claim_is_current(&stale, 20_000)
            .await
            .expect("stale current-claim query"),
        "a stale epoch is never current"
    );
    assert!(
        store
            .claim_is_current(&current, claimed.lease.expires_ms)
            .await
            .expect("exact-boundary current-claim query"),
        "the exact lease expiry boundary remains current"
    );
    assert!(
        !store
            .claim_is_current(&current, claimed.lease.expires_ms + 1)
            .await
            .expect("expired current-claim query"),
        "an expired claim is not current before it is reclaimed"
    );
    assert!(
        store
            .worker_owns_run(&identity, &run, claimed.lease.expires_ms)
            .await
            .expect("registered Worker ownership query"),
        "the exact Worker incarnation owns the live run"
    );
    let stale_identity = WorkerIdentity::new(identity.worker_id.clone(), "replacement", 2);
    assert!(
        !store
            .worker_owns_run(&stale_identity, &run, 20_000)
            .await
            .expect("stale Worker ownership query"),
        "another incarnation never owns the run"
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
    assert!(
        !store
            .worker_owns_run(&identity, &run, 20_000)
            .await
            .expect("settled Worker ownership query"),
        "settlement removes Worker ownership"
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
        .claim_run(
            &run,
            "conformance-sandbox-a",
            LEASE_MS,
            30_000,
            &Default::default(),
        )
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
            &Default::default(),
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
        .claim_run(
            &awaiting,
            "conformance-completion",
            LEASE_MS,
            40_000,
            &Default::default(),
        )
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
        .claim_run(
            &done,
            "conformance-completion",
            LEASE_MS,
            40_000,
            &Default::default(),
        )
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
            .claim_run(
                &done,
                "conformance-replay",
                LEASE_MS,
                40_001,
                &Default::default(),
            )
            .await
            .expect("exact claim after replay")
            .is_none(),
        "completed run id does not resurrect through enqueue"
    );
    assert!(
        store
            .claim_new_run(
                request.clone(),
                "conformance-replay",
                LEASE_MS,
                40_001,
                &Default::default(),
            )
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
        credential_observations: Default::default(),
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

fn credential_dispatch(ns: &str, run: &str, thread: &str, holder: &PlaintextHolder) -> RunDispatch {
    let mut request = dispatch(ns, run, thread);
    request.activation.snapshot.resolved_spec.model_binding =
        awaken_runtime_contract::resolved::ResolvedModelCandidate::provider(
            ModelBinding::new(format!("{ns}-provider"), "model", "genai"),
            format!("{ns}-provider@1"),
            format!("{ns}-route@1"),
            ns,
            Some(CredentialAccess::new(
                CredentialRef {
                    id: format!("{ns}-credential"),
                    revision: 7,
                },
                CredentialMaterialSource::ControlPlaneReference,
                CredentialUsage::ProviderAdapter,
                CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::Forbidden),
            )),
            InferenceEndpoint {
                adapter_kind: "openai".into(),
                api_dialect: "open_ai_chat".into(),
                base_url: "https://provider.invalid/v1".into(),
                upstream_model: "model".into(),
            },
        );
    request.inference_plaintext_holder = Some(holder.clone());
    request
}

fn credential_worker(ns: &str, holder: &PlaintextHolder, capable: bool) -> WorkerSnapshot {
    let mut manifest = WorkerManifest::default();
    if capable {
        let realization = CredentialRealizationCapabilities {
            holders: [holder.clone()].into_iter().collect(),
            material_sources: [CredentialMaterialSource::ControlPlaneReference]
                .into_iter()
                .collect(),
            realization_kinds: [CredentialRealizationKind::WorkerProviderAdapter]
                .into_iter()
                .collect(),
            recipient_bound_envelopes: false,
            alternatives: Vec::new(),
        };
        manifest.capabilities.insert(
            realization
                .manifest_capability()
                .expect("credential capabilities serialize")
                .expect("non-empty credential capabilities emit one manifest entry"),
        );
    }
    WorkerSnapshot {
        identity: WorkerIdentity::new(format!("{ns}-credential-worker-{capable}"), "boot", 1),
        capability_fingerprint: manifest
            .fingerprint()
            .expect("credential Worker manifest fingerprints"),
        manifest,
        state: WorkerState::Ready,
        in_flight: 0,
        credential_observations: Default::default(),
        expires_at_ms: 100_000,
    }
}

fn run_id(ns: &str, suffix: &str) -> RunId {
    RunId(format!("{ns}-{suffix}"))
}

fn thread_id(ns: &str, suffix: &str) -> ThreadId {
    ThreadId(format!("{ns}-{suffix}"))
}
