// @formal surface exclude=test_support_crate
// @formal owner crates/platform/dispatch/awaken-dispatch-testkit/src/lib.rs
// @formal reason: Reusable dispatch conformance fixtures and fake executors; it is not linked into a
// @formal reason: product execution surface.
//! Reusable behavioural conformance for durable dispatch implementations.
//!
//! Backends and decorators call these functions from their own integration tests.
//! The suite intentionally depends only on the public contract: passing it proves a
//! wrapper preserves the worker-visible claim, fencing, and recovery semantics rather
//! than merely delegating a convenient subset of methods.

mod observed_clock;
pub use observed_clock::ObservedStoreClock;
pub mod worker_dispatch;

use awaken_agent_contract::agent::delegation::DelegationOrigin;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::credential::{
    CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef,
    CredentialUsage, ModelExposurePolicy, PlaintextBoundary, PlaintextHolder,
};
use awaken_credential_contract::CredentialRealizationReceipt;
use awaken_credential_contract::{CredentialRealizationCapabilities, CredentialRealizationKind};
use awaken_dispatch_contract::operational::{
    DispatchCursor, DispatchOperation, DispatchOperationalFeed, LeaseLossReason,
};
use awaken_dispatch_contract::{DispatchAdmissionShape, RunDispatch, SessionRuntimeEnvelope};
use awaken_dispatch_contract::{
    dispatch::AttemptAdmission, dispatch::CasOutcome, dispatch::ContinuationAdmission,
    dispatch::DispatchError, dispatch::DispatchOutcome, dispatch::DispatchState, dispatch::Inbox,
    dispatch::Outbox, dispatch::PendingInput, dispatch::SessionChildAdmission,
    dispatch::SessionContinuationOutbox, dispatch::SessionRunReservationActivation,
    dispatch::SessionRunReservationOutcome, dispatch::SessionRunReservationResolution,
    dispatch::SettleOutcome, dispatch::SubmitOptions,
};
use awaken_runtime_contract::InferenceEndpoint;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_session_execution_contract::{AdmitSessionRun, SessionRunExecutionRequirements};
use awaken_session_types::{SessionRunCommandFingerprint, SessionRunReplacement};
use awaken_worker_contract::RunClaim;
use awaken_worker_contract::{
    LeastLoadedPolicy, PlacementRequirements, WORKER_LOCAL_CREDENTIALS_CAPABILITY,
    WorkerCredentialObservation, WorkerIdentity, WorkerManifest, WorkerSnapshot, WorkerState,
};

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

    use awaken_runtime_contract::activation::RunActivation;
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use awaken_runtime_contract::snapshot::{
        AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
    };
    use awaken_worker_contract::{
        RegisteredWorker, RegistryError, RegistryMutation, WorkerDirectory, WorkerHeartbeat,
        WorkerIdentity, WorkerManifest, WorkerObservationSource, WorkerRegistration,
        WorkerSnapshot, WorkerState,
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
    #[expect(
        clippy::disallowed_methods,
        reason = "the conformance host stamps and advances its fixture time on the host wall clock"
    )]
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

    /// Current wire-codec precondition shared by the existing HTTP and
    /// conformance fixtures. This advertises only the compiled prepared/journal
    /// formats, never execution, credentials, private Task support or stop proof.
    #[must_use]
    pub fn current_dispatch_codec_manifest() -> WorkerManifest {
        let mut manifest = WorkerManifest::default();
        manifest.runtime_protocol.min = 1;
        manifest.runtime_protocol.max = awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION;
        manifest.capabilities.extend([
            awaken_worker_contract::PREPARED_SESSION_DISPATCH_CAPABILITY.to_owned(),
            awaken_worker_contract::TOOL_RESULT_JOURNAL_CAPABILITY.to_owned(),
        ]);
        manifest
    }

    pub async fn ready_worker(worker_id: &str) -> (Arc<CurrentWorkerDirectory>, WorkerIdentity) {
        worker_in_state(worker_id, WorkerState::Ready).await
    }

    /// The same fixed live incarnation in another lifecycle state.
    pub async fn worker_in_state(
        worker_id: &str,
        state: WorkerState,
    ) -> (Arc<CurrentWorkerDirectory>, WorkerIdentity) {
        let record = fixed_record(worker_id, state, current_dispatch_codec_manifest());
        let identity = record.snapshot.identity.clone();
        (CurrentWorkerDirectory::holding(vec![record]), identity)
    }

    /// Several fixed Ready incarnations in one directory, in `worker_ids`
    /// order; each identity is [`fixed_identity`] of its Worker id.
    #[must_use]
    pub fn ready_workers(worker_ids: &[&str]) -> Arc<CurrentWorkerDirectory> {
        ready_workers_with_manifest(worker_ids, &current_dispatch_codec_manifest())
    }

    /// [`ready_workers`] offering `manifest` (for example with credential
    /// realization capabilities) instead of the codec manifest.
    #[must_use]
    pub fn ready_workers_with_manifest(
        worker_ids: &[&str],
        manifest: &WorkerManifest,
    ) -> Arc<CurrentWorkerDirectory> {
        CurrentWorkerDirectory::holding(
            worker_ids
                .iter()
                .map(|worker_id| fixed_record(worker_id, WorkerState::Ready, manifest.clone()))
                .collect(),
        )
    }

    /// The one incarnation the fixed directories register for `worker_id`.
    #[must_use]
    pub fn fixed_identity(worker_id: &str) -> WorkerIdentity {
        WorkerIdentity::new(worker_id, format!("{worker_id}-boot"), 1)
    }

    fn fixed_record(
        worker_id: &str,
        state: WorkerState,
        manifest: WorkerManifest,
    ) -> RegisteredWorker {
        RegisteredWorker {
            snapshot: WorkerSnapshot {
                identity: fixed_identity(worker_id),
                state,
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
            source_retention: Default::default(),
        }
    }

    /// The original fixed ready-incarnation fixture. Only its existing read
    /// leaf is faultable; no alternate directory or registration authority.
    pub struct CurrentWorkerDirectory {
        records: awaken_sync::LeafLock<Vec<RegisteredWorker>>,
        current_unavailable: std::sync::atomic::AtomicBool,
        current_reads: std::sync::atomic::AtomicUsize,
    }

    impl CurrentWorkerDirectory {
        fn holding(records: Vec<RegisteredWorker>) -> Arc<Self> {
            Arc::new(Self {
                records: awaken_sync::LeafLock::new(records),
                current_unavailable: std::sync::atomic::AtomicBool::new(false),
                current_reads: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        pub fn set_current_unavailable(&self, unavailable: bool) {
            self.current_unavailable
                .store(unavailable, std::sync::atomic::Ordering::SeqCst);
        }

        #[must_use]
        pub fn current_reads(&self) -> usize {
            self.current_reads.load(std::sync::atomic::Ordering::SeqCst)
        }

        /// The Worker's heartbeat now reports `in_flight` admitted work (a
        /// stale owner that still counts work it lost), which placement reads.
        pub fn set_in_flight(&self, worker_id: &str, in_flight: u32) {
            let mut records = self.records.lock();
            for record in records
                .iter_mut()
                .filter(|record| record.snapshot.identity.worker_id == worker_id)
            {
                record.snapshot.in_flight = in_flight;
            }
        }

        fn records(&self) -> Vec<RegisteredWorker> {
            self.records.lock().clone()
        }

        fn record(&self, worker_id: &str) -> Option<RegisteredWorker> {
            self.records()
                .into_iter()
                .find(|record| record.snapshot.identity.worker_id == worker_id)
        }
    }

    #[async_trait::async_trait]
    impl WorkerObservationSource for CurrentWorkerDirectory {
        async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
            Ok(self.records())
        }
    }

    #[async_trait::async_trait]
    impl awaken_worker_contract::WorkerCurrentSource for CurrentWorkerDirectory {
        async fn current(
            &self,
            worker_id: &str,
        ) -> Result<Option<RegisteredWorker>, RegistryError> {
            self.current_reads
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self
                .current_unavailable
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(RegistryError::Persistence(
                    "Registry current read is unavailable".into(),
                ));
            }
            Ok(self.record(worker_id))
        }
    }

    #[async_trait::async_trait]
    impl awaken_worker_contract::WorkerIncarnationSource for CurrentWorkerDirectory {
        async fn incarnation(
            &self,
            identity: &awaken_worker_contract::WorkerIdentity,
        ) -> Result<
            Option<awaken_worker_contract::RegisteredWorker>,
            awaken_worker_contract::RegistryError,
        > {
            awaken_worker_contract::read_current_incarnation(self, identity).await
        }
    }

    #[async_trait::async_trait]
    impl WorkerDirectory for CurrentWorkerDirectory {
        async fn lock_incarnation(
            &self,
            _identity: &WorkerIdentity,
        ) -> Result<Option<awaken_worker_contract::WorkerIncarnationGuard>, RegistryError> {
            Err(RegistryError::Persistence(
                "fixture forbids physical admission".into(),
            ))
        }

        async fn register(
            &self,
            registration: WorkerRegistration,
            _now_ms: u64,
            _ttl_ms: u64,
        ) -> Result<RegisteredWorker, RegistryError> {
            // A fixed fixture: it returns its own record, never a new one.
            Ok(self
                .record(&registration.worker_id)
                .or_else(|| self.records().into_iter().next())
                .expect("fixture directory holds at least one Worker"))
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
            _receipt: awaken_worker_contract::WorkerProcessExit,
        ) -> Result<RegistryMutation, RegistryError> {
            Err(RegistryError::Persistence(
                "process-exit persistence is outside this read-oriented fixture".into(),
            ))
        }
    }

    #[cfg(test)]
    mod codec_manifest_tests {
        use super::*;

        #[tokio::test]
        async fn shared_manifest_matches_intrinsic_codecs_and_both_ready_consumers() {
            // Cause/effect table: C1 current prepared Native row vs unmarked
            // legacy row; C2 immutable offered manifest complete/None/each
            // omitted capability. R1 current+complete => common admission;
            // R2 legacy|None|any omission => refusal. Compare the advertised
            // SET/values to the actual production intrinsic marker compiler,
            // not a second expected token list. R3 both existing Ready fixture
            // constructors retain this SAME complete value and fingerprint.
            let manifest = current_dispatch_codec_manifest();
            let legacy = awaken_dispatch_contract::RunDispatch::new(activation("codec-manifest"));
            let current = legacy
                .clone()
                .with_prepared_session_command_identity()
                .with_worker_codec_requirements();
            assert_eq!(
                manifest.capabilities, current.placement.required_capabilities,
                "R1 actual intrinsic compiler"
            );
            assert!(
                current
                    .validate_worker_wire_codec_requirements(Some(&manifest))
                    .is_ok(),
                "R1"
            );
            assert!(
                legacy
                    .validate_worker_wire_codec_requirements(Some(&manifest))
                    .is_err(),
                "R2 unmarked row"
            );
            assert!(
                current
                    .validate_worker_wire_codec_requirements(None)
                    .is_err(),
                "R2 absent offered manifest"
            );
            for capability in &manifest.capabilities {
                let mut omitted = manifest.clone();
                omitted.capabilities.remove(capability);
                assert!(
                    current
                        .validate_worker_wire_codec_requirements(Some(&omitted))
                        .is_err(),
                    "R2 missing {capability}"
                );
            }
            let conformance = crate::ready_dispatch_worker("codec-manifest", "conformance");
            assert_eq!(
                conformance.manifest, manifest,
                "R3 existing conformance consumer"
            );
            let (directory, identity) = ready_worker("codec-manifest-http").await;
            let workers = directory.list().await.unwrap();
            assert_eq!(workers.len(), 1);
            assert_eq!(workers[0].snapshot.identity, identity);
            assert_eq!(
                workers[0].snapshot.manifest, manifest,
                "R3 existing HTTP consumer"
            );
            assert_eq!(
                workers[0].snapshot.capability_fingerprint,
                manifest.fingerprint().unwrap(),
                "R3 same registered manifest fingerprint"
            );
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

    #[expect(
        clippy::disallowed_methods,
        reason = "the conformance host stamps and advances its fixture time on the host wall clock"
    )]
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
/// | Cancel request then settle | completion keeps the cancel fact | completion K1–K3 |
/// | Command replay after Done | no new runnable row | caller identity / reservation SR6 |
/// | Terminal repair crash | repair epoch advances; one completion | terminal recovery T7/T8 |
/// | Missing physical cleanup ACK | occupied slot retained | occupied recovery |
pub async fn assert_dispatch_conformance<S: ConformanceDispatch + Inbox + Outbox>(
    store: &S,
    namespace: &str,
    capabilities: ConformanceCapabilities,
) {
    assert_dispatch_conformance_with_clock(store, namespace, capabilities, &AuthoritativeWallClock)
        .await;
}

pub async fn assert_dispatch_conformance_with_clock<S: ConformanceDispatch + Inbox + Outbox>(
    store: &S,
    namespace: &str,
    capabilities: ConformanceCapabilities,
    clock: &dyn ConformanceClock,
) {
    heap_rule(|| open_run_single_writer_is_uniform(store, namespace, clock)).await;
    if capabilities.local_commit_guard {
        heap_rule(|| local_claims_skip_remote_only_work(store, namespace, clock)).await;
    }
    heap_rule(|| occupied_recovery_does_not_starve_other_threads(store, namespace, clock)).await;
    heap_rule(|| exact_claim_recovery_and_fencing(store, namespace, clock)).await;
    heap_rule(|| {
        settlement_fence_and_pending_input_are_atomic(store, namespace, capabilities, clock)
    })
    .await;
    heap_rule(|| retry_exhaustion_claim_is_atomic_and_policy_exact(store, namespace, clock)).await;
    heap_rule(|| failed_attempts_spend_the_budget_and_exhaust_like_a_crash(store, namespace)).await;
    heap_rule(|| completion_retains_the_settled_cancel_request(store, namespace)).await;
    heap_rule(|| {
        task_sources_share_the_original_done_transaction(store, namespace, capabilities, clock)
    })
    .await;
    heap_rule(|| {
        attempt_credentials_are_atomic_and_epoch_fenced(store, namespace, capabilities, clock)
    })
    .await;
    heap_rule(|| incompatible_credentials_do_not_poison_broad_claims(store, namespace, clock))
        .await;
    heap_rule(|| parent_mediated_commands_are_atomic(store, namespace, clock)).await;
    heap_rule(|| caller_owned_run_identity_is_exact(store, namespace, clock)).await;
    heap_rule(|| {
        upgrade_window_retry_replays_rows_written_before_typed_requirements(store, namespace, clock)
    })
    .await;
    heap_rule(|| {
        session_run_reservation_is_atomic_and_recoverable(store, namespace, capabilities, clock)
    })
    .await;
    heap_rule(|| current_claim_guard_is_exact(store, namespace, capabilities, clock)).await;
    heap_rule(|| sandbox_binding_survives_recovery(store, namespace, capabilities, clock)).await;
    heap_rule(|| {
        completion_is_atomic_and_prevents_resurrection(store, namespace, capabilities, clock)
    })
    .await;
    if capabilities.completion_events {
        heap_rule(|| committed_terminal_recovery_reuses_fenced_settlement(store, namespace, clock))
            .await;
        heap_rule(|| {
            deferred_pending_terminal_recovery_obeys_retry_deadline(store, namespace, clock)
        })
        .await;
    }
    // This rule intentionally leaves 25 distinct unarchived child Threads as its
    // postcondition, so run it after every conformance rule that broadly claims
    // queue work.
    heap_rule(|| {
        task_attention_identity_survives_live_and_completed(store, namespace, capabilities, clock)
    })
    .await;
    heap_rule(|| session_child_admission_is_atomic_and_bounded(store, namespace, clock)).await;
}

/// Run one conformance rule behind its own heap boundary. An unoptimized poll
/// frame reserves a slot for every rule future the suite awaits inline, and
/// the suite's sum overflowed a 2 MiB test thread once every rule ran to its
/// end; the suite now holds one pointer per rule instead.
fn heap_rule<'a, F>(
    rule: impl FnOnce() -> F,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>>
where
    F: std::future::Future<Output = ()> + 'a,
{
    Box::pin(rule())
}

/// The one store shape the Session continuation journeys drive as a trait
/// object: the Control queue, inbox and outbox roles plus the atomic Session
/// continuation write. This conformance suite is its only consumer.
pub trait DurableControlDispatch:
    ConformanceDispatch + Inbox + Outbox + SessionContinuationOutbox
{
}

impl<T: ConformanceDispatch + Inbox + Outbox + SessionContinuationOutbox> DurableControlDispatch
    for T
{
}

include!("conformance/dispatch_admission.rs");

include!("conformance/session_messages.rs");

include!("conformance/recovery_credentials.rs");

include!("conformance/operations_claims.rs");

include!("conformance/reservations.rs");
include!("conformance/session_run_admission.rs");
include!("conformance/task_attention.rs");
include!("conformance/task_source.rs");

include!("conformance/fixtures.rs");

include!("conformance/reliability.rs");

include!("conformance/upgrade_window.rs");

include!("conformance/failed_attempt_budget.rs");

include!("conformance/completion_cancellation.rs");

use awaken_dispatch_contract::{
    DispatchAdministration, DispatchAttemptControl, DispatchClaimOwnership, DispatchClaims,
    DispatchCommitAuthority, DispatchControlClaims, DispatchControlReads, DispatchEnqueue,
    DispatchExecution, DispatchMaintenance, DispatchReads, DispatchSettlement,
    SessionRunReservations,
};
pub trait ConformanceDispatch:
    DispatchEnqueue
    + DispatchAdministration
    + DispatchClaims
    + DispatchExecution
    + DispatchClaimOwnership
    + DispatchCommitAuthority
    + DispatchSettlement
    + DispatchReads
    + DispatchMaintenance
    + SessionRunReservations
    + DispatchAttemptControl
    + DispatchControlClaims
    + DispatchControlReads
{
}
impl<
    T: DispatchEnqueue
        + DispatchAdministration
        + DispatchClaims
        + DispatchExecution
        + DispatchClaimOwnership
        + DispatchCommitAuthority
        + DispatchSettlement
        + DispatchReads
        + DispatchMaintenance
        + SessionRunReservations
        + DispatchAttemptControl
        + DispatchControlClaims
        + DispatchControlReads,
> ConformanceDispatch for T
{
}
