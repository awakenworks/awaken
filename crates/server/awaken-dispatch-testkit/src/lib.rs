// @formal surface exclude=test_support_crate
// @formal owner crates/server/awaken-dispatch-testkit/src/lib.rs
// @formal reason: Reusable dispatch conformance fixtures and fake executors; it is not linked into a
// @formal reason: product execution surface.
//! Reusable behavioural conformance for durable dispatch implementations.
//!
//! Backends and decorators call these functions from their own integration tests.
//! The suite intentionally depends only on the public contract: passing it proves a
//! wrapper preserves the worker-visible claim, fencing, and recovery semantics rather
//! than merely delegating a convenient subset of methods.

use awaken_agent_contract::agent::delegation::DelegationOrigin;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_credential_contract::{CredentialRealizationCapabilities, CredentialRealizationKind};
use awaken_dispatch_contract::dispatch::{
    AttemptAdmission, CasOutcome, ContinuationAdmission, CredentialRealizationReceipt,
    DispatchError, DispatchOutcome, DispatchState, Inbox, Outbox, PendingInput, RunClaim,
    SessionChildAdmission, SessionContinuationOutbox, SessionRunReservationActivation,
    SessionRunReservationOutcome, SessionRunReservationResolution, SettleOutcome, SubmitOptions,
};
use awaken_dispatch_contract::operational::{
    DispatchCursor, DispatchOperation, DispatchOperationalFeed, LeaseLossReason,
};
use awaken_dispatch_contract::{
    ControlDispatchQueue, DispatchAdmissionShape, RunDispatch, SessionRunReplacement,
    SessionRuntimeEnvelope,
};
use awaken_dispatch_contract::{
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
    CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef,
    CredentialUsage, InferenceEndpoint, ModelExposurePolicy, PlaintextBoundary, PlaintextHolder,
};
use awaken_session_execution_contract::{AdmitSessionRun, SessionRunExecutionRequirements};
use awaken_session_types::SessionRunCommandFingerprint;

/// Minimum live lease used by cross-backend conformance journeys. A live
/// database clock keeps moving while an in-process logical clock does not, so
/// integration assertions must not use scheduler-sized (single-digit ms)
/// leases as if they were controllable logical time.
pub const LEASE_MS: u64 = 1_000;

/// Shared fixtures for real-HTTP Worker boundary tests. Keeping these here
/// prevents the Run Ingress and Resource HTTP crates from maintaining parallel
/// worker-directory and activation implementations.
pub mod worker_http {
    use std::sync::Arc;

    use awaken_dispatch_contract::{
        RegisteredWorker, RegistryError, RegistryMutation, WorkerDirectory, WorkerHeartbeat,
        WorkerIdentity, WorkerManifest, WorkerObservationSource, WorkerRegistration,
        WorkerSnapshot, WorkerState,
    };
    use awaken_runtime_contract::activation::RunActivation;
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use awaken_runtime_contract::snapshot::{
        AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
    };

    pub async fn serve(app: axum::Router) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind Worker HTTP fixture");
        let address = listener.local_addr().expect("Worker HTTP fixture address");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve Worker HTTP fixture");
        });
        address
    }

    #[must_use]
    pub fn unix_now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_millis() as u64
    }

    #[must_use]
    pub fn activation(tag: &str) -> RunActivation {
        RunActivation::new(
            awaken_agent_contract::agent::run::Id(format!("run-{tag}")),
            awaken_agent_contract::agent::thread::Id(format!("thread-{tag}")),
            ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId(format!("snapshot-{tag}")),
                metadata: Default::default(),
                root_agent_id: AgentId(format!("agent-{tag}")),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: CatalogFingerprint(format!("catalog-{tag}")),
                    instructions: "test Worker boundary".into(),
                    max_steps: Some(1),
                    delegation_limits: Default::default(),
                    model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                        ModelBinding::new("test", "model", "native"),
                    ),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint: CatalogFingerprint(format!("snapshot-{tag}-fingerprint")),
            },
            Vec::new(),
        )
    }

    pub async fn ready_worker(worker_id: &str) -> (Arc<dyn WorkerDirectory>, WorkerIdentity) {
        let mut manifest = WorkerManifest::default();
        manifest.runtime_protocol.min = 1;
        manifest.runtime_protocol.max =
            awaken_dispatch_contract::CURRENT_RUNTIME_PROTOCOL_VERSION;
        let identity = WorkerIdentity::new(worker_id, format!("{worker_id}-boot"), 1);
        let registered = RegisteredWorker {
            snapshot: WorkerSnapshot {
                identity: identity.clone(),
                state: WorkerState::Ready,
                capability_fingerprint: manifest
                    .fingerprint()
                    .expect("fixture manifest fingerprints"),
                manifest,
                in_flight: 0,
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: Default::default(),
                expires_at_ms: u64::MAX,
            },
            heartbeat_sequence: 1,
            observation_sequence: 0,
            registered_at_ms: 0,
            heartbeat_at_ms: 0,
            drain_deadline_ms: None,
            process_exit: None,
            create_ledger: None,
        };
        (Arc::new(CurrentWorkerDirectory(registered)), identity)
    }

    struct CurrentWorkerDirectory(RegisteredWorker);

    #[async_trait::async_trait]
    impl WorkerObservationSource for CurrentWorkerDirectory {
        async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
            Ok(vec![self.0.clone()])
        }
    }

    #[async_trait::async_trait]
    impl WorkerDirectory for CurrentWorkerDirectory {
        async fn lock_incarnation(
            &self,
            _identity: &WorkerIdentity,
        ) -> Result<Option<awaken_dispatch_contract::WorkerIncarnationGuard>, RegistryError>
        {
            Err(RegistryError::Persistence(
                "fixture forbids physical admission".into(),
            ))
        }

        async fn register(
            &self,
            _registration: WorkerRegistration,
            _now_ms: u64,
            _ttl_ms: u64,
        ) -> Result<RegisteredWorker, RegistryError> {
            Ok(self.0.clone())
        }

        async fn heartbeat(
            &self,
            _identity: &WorkerIdentity,
            _heartbeat: WorkerHeartbeat,
            _now_ms: u64,
            _ttl_ms: u64,
        ) -> Result<RegistryMutation, RegistryError> {
            Ok(RegistryMutation::NotFound)
        }

        async fn begin_drain(
            &self,
            _identity: &WorkerIdentity,
            _deadline_ms: u64,
        ) -> Result<RegistryMutation, RegistryError> {
            Ok(RegistryMutation::NotFound)
        }

        async fn mark_quiesced(
            &self,
            _identity: &WorkerIdentity,
        ) -> Result<RegistryMutation, RegistryError> {
            Ok(RegistryMutation::NotFound)
        }

        async fn deregister(
            &self,
            _identity: &WorkerIdentity,
        ) -> Result<RegistryMutation, RegistryError> {
            Ok(RegistryMutation::NotFound)
        }

        async fn current(
            &self,
            worker_id: &str,
        ) -> Result<Option<RegisteredWorker>, RegistryError> {
            Ok((worker_id == self.0.snapshot.identity.worker_id).then(|| self.0.clone()))
        }

        async fn expire_incarnation(
            &self,
            _identity: &WorkerIdentity,
            _now_ms: u64,
        ) -> Result<RegistryMutation, RegistryError> {
            Ok(RegistryMutation::InvalidTransition)
        }

        async fn record_process_exit(
            &self,
            _identity: &WorkerIdentity,
            _receipt: awaken_dispatch_contract::WorkerProcessExit,
        ) -> Result<RegistryMutation, RegistryError> {
            Err(RegistryError::Persistence(
                "process-exit persistence is outside this read-oriented fixture".into(),
            ))
        }
    }
}

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

/// The suite's handle on the authority clock the store under test reads from.
/// A direct store is built over an injected `ManualClock`, so `set` moves the
/// exact time its rows are judged against; a live database/server clock cannot
/// be moved and answers in real time instead.
#[async_trait::async_trait]
pub trait ConformanceClock: Send + Sync {
    fn set(&self, now_ms: u64);

    /// Whether the suite can hold the authority clock at an exact millisecond.
    /// Live database/server clocks still exercise recovery, but their moving
    /// boundary is covered by the shared pure transition tests instead of a
    /// timing-sensitive integration assertion.
    fn exact_boundary_is_controllable(&self) -> bool;

    /// Advance the authority strictly beyond a persisted lease deadline.
    async fn advance_past(&self, deadline_ms: u64);
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AuthoritativeWallClock;

#[async_trait::async_trait]
impl ConformanceClock for AuthoritativeWallClock {
    fn set(&self, _now_ms: u64) {}

    fn exact_boundary_is_controllable(&self) -> bool {
        false
    }

    async fn advance_past(&self, deadline_ms: u64) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("conformance host clock is after the Unix epoch")
            .as_millis() as u64;
        // The margin absorbs host/database clock skew without weakening the
        // tested predicate: the subsequent claim must still recover the exact
        // persisted row and advance its fencing epoch.
        let wait_ms = deadline_ms.saturating_sub(now_ms).saturating_add(25);
        tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
    }
}

#[async_trait::async_trait]
impl<F> ConformanceClock for F
where
    F: Fn(u64) + Send + Sync,
{
    fn set(&self, now_ms: u64) {
        self(now_ms);
    }

    fn exact_boundary_is_controllable(&self) -> bool {
        true
    }

    async fn advance_past(&self, deadline_ms: u64) {
        self(deadline_ms.saturating_add(1));
    }
}

/// A direct store's injected clock together with the suite handle that moves it.
///
/// `clock()` is the handle the store is constructed over; the value itself is
/// the [`ConformanceClock`] the suite drives, so `set` moves the exact
/// millisecond that store's rows are judged against.
#[derive(Debug, Clone, Default)]
pub struct ManualStoreClock(std::sync::Arc<awaken_lease_testkit::ManualClock>);

impl ManualStoreClock {
    #[must_use]
    pub fn new(now_ms: u64) -> Self {
        Self(std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(
            now_ms,
        )))
    }

    /// The handle to inject into the store under test.
    #[must_use]
    pub fn clock(&self) -> std::sync::Arc<dyn awaken_time::WallClock> {
        self.0.clone()
    }
}

#[async_trait::async_trait]
impl ConformanceClock for ManualStoreClock {
    fn set(&self, now_ms: u64) {
        self.0.set(now_ms);
    }

    fn exact_boundary_is_controllable(&self) -> bool {
        true
    }

    async fn advance_past(&self, deadline_ms: u64) {
        self.0.set(deadline_ms.saturating_add(1));
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
///
/// Failure coverage is shared by every backend binding below:
///
/// | Failure boundary | Required durable observation | Owning rule |
/// |---|---|---|
/// | Cancellation before activation | no unreceipted execution | Session reservation SR7 |
/// | Worker crash at lease expiry | newer claim; stale owner fenced | exact claim recovery |
/// | Settlement response lost | one retained completion; retry fenced | completion replay |
/// | Command replay after Done | no new runnable row | caller identity / reservation SR6 |
/// | Terminal repair crash | repair epoch advances; one completion | terminal recovery T7/T8 |
/// | Missing physical cleanup ACK | occupied slot retained | occupied recovery |
pub async fn assert_dispatch_conformance<S: ControlDispatchQueue + Inbox + Outbox>(
    store: &S,
    namespace: &str,
    capabilities: ConformanceCapabilities,
) {
    assert_dispatch_conformance_with_clock(store, namespace, capabilities, &AuthoritativeWallClock)
        .await;
}

pub async fn assert_dispatch_conformance_with_clock<S: ControlDispatchQueue + Inbox + Outbox>(
    store: &S,
    namespace: &str,
    capabilities: ConformanceCapabilities,
    clock: &dyn ConformanceClock,
) {
    open_run_single_writer_is_uniform(store, namespace, clock).await;
    if capabilities.local_commit_guard {
        local_claims_skip_remote_only_work(store, namespace, clock).await;
    }
    occupied_recovery_does_not_starve_other_threads(store, namespace, clock).await;
    exact_claim_recovery_and_fencing(store, namespace, clock).await;
    settlement_fence_and_pending_input_are_atomic(store, namespace, capabilities, clock).await;
    retry_exhaustion_claim_is_atomic_and_policy_exact(store, namespace, clock).await;
    attempt_credentials_are_atomic_and_epoch_fenced(store, namespace, capabilities, clock).await;
    incompatible_credentials_do_not_poison_broad_claims(store, namespace, clock).await;
    parent_mediated_commands_are_atomic(store, namespace, clock).await;
    caller_owned_run_identity_is_exact(store, namespace, clock).await;
    session_run_reservation_is_atomic_and_recoverable(store, namespace, capabilities, clock).await;
    current_claim_guard_is_exact(store, namespace, capabilities, clock).await;
    sandbox_binding_survives_recovery(store, namespace, capabilities, clock).await;
    completion_is_atomic_and_prevents_resurrection(store, namespace, capabilities, clock).await;
    if capabilities.completion_events {
        committed_terminal_recovery_reuses_fenced_settlement(store, namespace, clock).await;
        deferred_pending_terminal_recovery_obeys_retry_deadline(store, namespace, clock).await;
    }
    // This rule intentionally leaves 25 distinct unarchived child Threads as its
    // postcondition, so run it after every conformance rule that broadly claims
    // queue work.
    session_child_admission_is_atomic_and_bounded(store, namespace, clock).await;
}

/// The one store shape the Session continuation journeys drive as a trait
/// object: the Control queue, inbox and outbox roles plus the atomic Session
/// continuation write. This conformance suite is its only consumer.
pub trait DurableControlDispatch:
    ControlDispatchQueue + Inbox + Outbox + SessionContinuationOutbox
{
}

impl<T: ControlDispatchQueue + Inbox + Outbox + SessionContinuationOutbox> DurableControlDispatch
    for T
{
}

include!("conformance/dispatch_admission.rs");

include!("conformance/session_messages.rs");

include!("conformance/recovery_credentials.rs");

include!("conformance/operations_claims.rs");

include!("conformance/reservations.rs");

include!("conformance/fixtures.rs");

include!("conformance/reliability.rs");
