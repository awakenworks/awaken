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
    CasOutcome, ContinuationAdmission, CredentialRealizationReceipt, Dispatch, DispatchOutcome,
    DispatchQueue, PendingInput, RunClaim, SessionChildAdmission, SessionRunReservationActivation,
    SessionRunReservationOutcome, SessionRunReservationResolution, SettleOutcome, SubmitOptions,
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

/// Shared fixtures for real-HTTP Worker boundary tests. Keeping these here
/// prevents the Run Ingress and Resource HTTP crates from maintaining parallel
/// worker-directory and activation implementations.
pub mod worker_http {
    use std::sync::Arc;

    use awaken_run_ingress_contract::{
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
                    max_steps: 1,
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
        let manifest = WorkerManifest::default();
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

        async fn expire(&self, _now_ms: u64) -> Result<Vec<WorkerIdentity>, RegistryError> {
            Ok(Vec::new())
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

/// Optional bridge for transports whose authoritative clock lives on the server.
/// Direct stores ignore it because their `now_ms` command argument is already the
/// clock input under test.
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
pub struct DirectCommandClock;

#[async_trait::async_trait]
impl ConformanceClock for DirectCommandClock {
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
    open_run_single_writer_is_uniform(store, namespace).await;
    if capabilities.local_commit_guard {
        local_claims_skip_remote_only_work(store, namespace).await;
    }
    exact_claim_recovery_and_fencing(store, namespace, clock).await;
    retry_exhaustion_claim_is_atomic_and_policy_exact(store, namespace, clock).await;
    attempt_credentials_are_atomic_and_epoch_fenced(store, namespace, capabilities, clock).await;
    incompatible_credentials_do_not_poison_broad_claims(store, namespace).await;
    parent_mediated_commands_are_atomic(store, namespace, clock).await;
    caller_owned_run_identity_is_exact(store, namespace).await;
    session_run_reservation_is_atomic_and_recoverable(store, namespace, capabilities, clock).await;
    current_claim_guard_is_exact(store, namespace, capabilities, clock).await;
    sandbox_binding_survives_recovery(store, namespace, capabilities, clock).await;
    completion_is_atomic_and_prevents_resurrection(store, namespace, capabilities, clock).await;
    if capabilities.completion_events {
        committed_terminal_recovery_reuses_fenced_settlement(store, namespace, clock).await;
    }
    // This rule intentionally leaves 25 distinct unarchived child Threads as its
    // postcondition, so run it after every conformance rule that broadly claims
    // queue work.
    session_child_admission_is_atomic_and_bounded(store, namespace).await;
}

include!("conformance/dispatch_admission.rs");

include!("conformance/session_messages.rs");

include!("conformance/recovery_credentials.rs");

include!("conformance/operations_claims.rs");

include!("conformance/reservations.rs");

include!("conformance/fixtures.rs");
