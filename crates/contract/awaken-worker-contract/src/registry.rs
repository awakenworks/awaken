// @formal entrypoint observation_reconcile.evidence_aba_has_fresh_fence
// @formal entrypoint observation_reconcile.failed_refresh_does_not_advance_fence
// @formal entrypoint observation_reconcile.skipped_epoch_was_published_or_superseded
// @formal entrypoint observation_reconcile.unchanged_evidence_coalesces
// @formal entrypoint sandbox.k8s_process_observation_requires_complete_exit
// @formal entrypoint worker.drain_and_replacement_races_preserve_registry_authority
// @formal entrypoint worker.incarnation_generation_fences_stale_processes
// @formal entrypoint worker_credential.probe_failure_does_not_drain_worker
// @formal entrypoint worker_credential.stale_heartbeat_never_rolls_back_revision
use std::collections::BTreeSet;

use async_trait::async_trait;
use awaken_provisioning_contract::SandboxCapacityShapeId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::manifest::WorkerManifest;
use crate::observation::{
    WorkerAcpCapabilityObservation, WorkerCredentialObservation, dynamic_evidence_admits,
};
use crate::requirements::{PlacementRequirements, WorkerRecoveryMode, can_claim};

/// One concrete worker process. `worker_id` names the logical slot;
/// `incarnation_id` changes on every boot; `generation` is allocated durably by
/// the registry. Reusing a worker id therefore never reuses execution authority.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WorkerIdentity {
    pub worker_id: String,
    pub incarnation_id: String,
    pub generation: u64,
}

/// The 128-bit lowercase hexadecimal boot coordinate shared by a trusted
/// local process launcher and the Worker it starts. Other Worker transports
/// retain their own incarnation format and never call this predicate.
#[must_use]
pub fn is_local_process_incarnation(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl WorkerIdentity {
    #[must_use]
    pub fn new(
        worker_id: impl Into<String>,
        incarnation_id: impl Into<String>,
        generation: u64,
    ) -> Self {
        Self {
            worker_id: worker_id.into(),
            incarnation_id: incarnation_id.into(),
            generation,
        }
    }

    /// Stable owner vocabulary for the existing dispatch lease. It includes the
    /// boot identity, so bulk renewal cannot accidentally renew a replacement's
    /// or predecessor's leases even before stores gain typed identity columns.
    #[must_use]
    pub fn lease_owner(&self) -> String {
        format!(
            "{}:{}:{}",
            self.worker_id, self.generation, self.incarnation_id
        )
    }

    /// Recover an exact historical identity from a Session realization lease
    /// without splitting a Kubernetes Worker id on its own colons. Round-trip
    /// equality rejects noncanonical generations; the caller must still read
    /// this exact identity from Registry to reject an invented boot value.
    #[must_use]
    pub fn from_lease_owner(worker_id: &str, lease_owner: &str) -> Option<Self> {
        if worker_id.trim().is_empty() || worker_id.trim() != worker_id {
            return None;
        }
        let suffix = lease_owner.strip_prefix(worker_id)?.strip_prefix(':')?;
        let (generation, incarnation_id) = suffix.split_once(':')?;
        let generation = generation.parse::<u64>().ok().filter(|value| *value > 0)?;
        if incarnation_id.trim().is_empty() || incarnation_id.trim() != incarnation_id {
            return None;
        }
        let identity = Self::new(worker_id, incarnation_id, generation);
        (identity.lease_owner() == lease_owner).then_some(identity)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    Starting,
    Ready,
    Draining,
    Quiesced,
    Dead,
}

impl WorkerState {
    #[must_use]
    pub const fn accepts_work(self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// State of the asynchronous dynamic-evidence probe relative to process
/// startup. Probe evidence affects placement, never process liveness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DynamicEvidenceProbeState {
    Pending,
    Succeeded,
    Failed,
}

/// Readiness policy after registration and runtime assembly have succeeded.
///
/// The probe state is explicit so the independence claim is executable and
/// exhaustively provable. Callers still publish no dynamic evidence until a
/// successful probe; [`dynamic_evidence_admits`] enforces that claim fence.
#[must_use]
pub const fn process_ready_after_startup(_probe: DynamicEvidenceProbeState) -> bool {
    true
}

/// Registry view consumed by placement. Live executor/channel handles are never
/// stored here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerSnapshot {
    pub identity: WorkerIdentity,
    pub state: WorkerState,
    pub manifest: WorkerManifest,
    pub capability_fingerprint: String,
    pub in_flight: u32,
    /// Exact mount-less Environment shapes currently ready in this incarnation's
    /// never-used capacity. These are ephemeral receipts, not capabilities.
    #[serde(default)]
    pub warm_environment_shapes: BTreeSet<SandboxCapacityShapeId>,
    /// Latest non-secret credential observations reported by this incarnation.
    /// The set is deliberately outside the immutable manifest: local login or
    /// revocation may change while the worker process remains alive.
    #[serde(default)]
    pub credential_observations: BTreeSet<WorkerCredentialObservation>,
    #[serde(default)]
    pub acp_capability_observations: Vec<WorkerAcpCapabilityObservation>,
    pub expires_at_ms: u64,
}

/// Durable, non-secret record of the worker incarnation selected for one claim.
/// The dispatch lease epoch remains the fencing token; this record explains who
/// received that epoch and which immutable capability set was evaluated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerAssignment {
    pub identity: WorkerIdentity,
    pub capability_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AssignmentRejection {
    #[error("worker is not currently eligible for the pinned requirements")]
    Ineligible,
    #[error("the run forbids replacement by another worker incarnation")]
    ReplacementForbidden,
    #[error("replacement requires an existing sandbox binding")]
    SandboxContinuityUnavailable,
}

/// Heap-free recovery kernel shared by runtime admission and Kani. Returning a
/// typed reason (rather than a boolean) keeps fail-closed diagnostics identical
/// in the proof harness and the store claim paths.
#[must_use]
pub const fn assignment_recovery_rejection(
    replacing: bool,
    recovery: WorkerRecoveryMode,
    sandbox_bound: bool,
) -> Option<AssignmentRejection> {
    if !replacing {
        return None;
    }
    match recovery {
        WorkerRecoveryMode::NeverReplace => Some(AssignmentRejection::ReplacementForbidden),
        WorkerRecoveryMode::RequireSandboxContinuity if !sandbox_bound => {
            Some(AssignmentRejection::SandboxContinuityUnavailable)
        }
        WorkerRecoveryMode::RequireSandboxContinuity
        | WorkerRecoveryMode::RebuildFromCommittedTruth => None,
    }
}

/// Shared admission kernel for initial placement, wake, and crash replacement.
pub fn can_assign(
    worker: &WorkerSnapshot,
    requirements: &PlacementRequirements,
    previous: Option<&WorkerAssignment>,
    sandbox_bound: bool,
    now_ms: u64,
) -> Result<(), AssignmentRejection> {
    if !worker.accepts(requirements, now_ms) {
        return Err(AssignmentRejection::Ineligible);
    }
    let replacing = previous.is_some_and(|prior| prior.identity != worker.identity);
    if let Some(rejection) =
        assignment_recovery_rejection(replacing, requirements.recovery, sandbox_bound)
    {
        Err(rejection)
    } else {
        Ok(())
    }
}

impl From<&WorkerSnapshot> for WorkerAssignment {
    fn from(snapshot: &WorkerSnapshot) -> Self {
        Self {
            identity: snapshot.identity.clone(),
            capability_fingerprint: snapshot.capability_fingerprint.clone(),
        }
    }
}

/// Durable registry record. Placement consumes `snapshot`; sequence/timestamps
/// remain control-plane concurrency and observability facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredWorker {
    pub snapshot: WorkerSnapshot,
    pub heartbeat_sequence: u64,
    /// Sequence of the latest accepted heartbeat that changed dynamic
    /// credential or ACP evidence. Unlike a content hash, this fence cannot
    /// return to an older value when evidence changes A -> B -> A.
    #[serde(default)]
    pub observation_sequence: u64,
    pub registered_at_ms: u64,
    pub heartbeat_at_ms: u64,
    pub drain_deadline_ms: Option<u64>,
    /// Immutable, provider-bound observation that this exact incarnation's
    /// whole local process domain exited. The Registry owns this Worker-level
    /// fact so every blocked attempt can reuse it after Pod evidence release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_exit: Option<WorkerProcessExit>,
    /// The one provider-owned Create request ledger frozen with this exact
    /// incarnation before registration can admit any execution. Historical
    /// rows without it cannot acquire a replacement ledger after effects may
    /// have started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_ledger: Option<WorkerCreateLedger>,
}

/// Secret-free provider identity of the independent Create request ledger.
/// The Registry binds it to the enclosing Worker incarnation; the provider
/// must reobserve this exact UID before using any ticket as closure evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerCreateLedger {
    provider_id: String,
    namespace: String,
    name: String,
    uid: String,
    configuration_fingerprint: String,
}

impl WorkerCreateLedger {
    pub fn new(
        provider_id: impl Into<String>,
        namespace: impl Into<String>,
        name: impl Into<String>,
        uid: impl Into<String>,
        configuration_fingerprint: impl Into<String>,
    ) -> Result<Self, RegistryError> {
        let ledger = Self {
            provider_id: provider_id.into(),
            namespace: namespace.into(),
            name: name.into(),
            uid: uid.into(),
            configuration_fingerprint: configuration_fingerprint.into(),
        };
        ledger.validate()?;
        Ok(ledger)
    }

    pub fn validate(&self) -> Result<(), RegistryError> {
        if [
            self.provider_id.as_str(),
            self.namespace.as_str(),
            self.name.as_str(),
            self.uid.as_str(),
            self.configuration_fingerprint.as_str(),
        ]
        .into_iter()
        .any(|part| part.trim().is_empty() || part.trim() != part)
        {
            return Err(RegistryError::InvalidCreateLedger);
        }
        Ok(())
    }

    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }
    pub fn namespace(&self) -> &str {
        &self.namespace
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn uid(&self) -> &str {
        &self.uid
    }
    pub fn configuration_fingerprint(&self) -> &str {
        &self.configuration_fingerprint
    }
}

/// Durable, non-secret exit evidence for one exact Worker incarnation.
///
/// Provider and execution coordinates are copied from the already-persisted
/// attempt binding. `evidence_ref` identifies the provider observation. This
/// receipt proves local process exit only; Dispatch still requires independent
/// complete-attempt evidence before retiring any physical attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerProcessExit {
    provider_id: String,
    execution_ref: String,
    evidence_ref: String,
}

impl WorkerProcessExit {
    pub fn new(
        provider_id: impl Into<String>,
        execution_ref: impl Into<String>,
        evidence_ref: impl Into<String>,
    ) -> Result<Self, RegistryError> {
        let receipt = Self {
            provider_id: provider_id.into(),
            execution_ref: execution_ref.into(),
            evidence_ref: evidence_ref.into(),
        };
        receipt.validate()?;
        Ok(receipt)
    }

    pub fn validate(&self) -> Result<(), RegistryError> {
        if self.provider_id.trim().is_empty()
            || self.execution_ref.trim().is_empty()
            || self.evidence_ref.trim().is_empty()
        {
            Err(RegistryError::InvalidProcessExit)
        } else {
            Ok(())
        }
    }

    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    pub fn execution_ref(&self) -> &str {
        &self.execution_ref
    }

    pub fn evidence_ref(&self) -> &str {
        &self.evidence_ref
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerRegistration {
    pub worker_id: String,
    pub incarnation_id: String,
    pub manifest: WorkerManifest,
    /// Set only by the trusted Coordinator adapter after provider I/O. A
    /// Worker request cannot supply or replace its own physical witness.
    #[serde(skip)]
    pub create_ledger: Option<WorkerCreateLedger>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerHeartbeat {
    pub sequence: u64,
    pub ready: bool,
    pub in_flight: u32,
    #[serde(default)]
    pub warm_environment_shapes: BTreeSet<SandboxCapacityShapeId>,
    /// Exact worker-private credential revisions currently materializable.
    /// No secret, local path, environment name, or broker token crosses here.
    #[serde(default)]
    pub credential_observations: BTreeSet<WorkerCredentialObservation>,
    #[serde(default)]
    pub acp_capability_observations: Vec<WorkerAcpCapabilityObservation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryMutation {
    Applied,
    NotFound,
    StaleIncarnation,
    StaleSequence,
    InvalidTransition,
}

/// Closed admission table for the sole durable Worker-process exit receipt.
/// Exact replay is Applied; a different receipt can never overwrite history.
#[must_use]
pub const fn worker_process_exit_admission(
    record_present: bool,
    identity_matches: bool,
    worker_closed: bool,
    existing_receipt_present: bool,
    existing_receipt_matches: bool,
) -> RegistryMutation {
    if !record_present {
        RegistryMutation::NotFound
    } else if !identity_matches {
        RegistryMutation::StaleIncarnation
    } else if !worker_closed || (existing_receipt_present && !existing_receipt_matches) {
        RegistryMutation::InvalidTransition
    } else {
        RegistryMutation::Applied
    }
}

/// Admission of a different incarnation from one atomic Registry history read.
/// Current-incarnation retries bypass this decision and never extend authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerRegistrationHistoryAdmission {
    Fresh,
    HistoricalIncarnation,
    IncompleteHistory,
}

#[must_use]
pub const fn worker_registration_history_admission(
    incarnation_seen: bool,
    history_complete: bool,
) -> WorkerRegistrationHistoryAdmission {
    if incarnation_seen {
        WorkerRegistrationHistoryAdmission::HistoricalIncarnation
    } else if !history_complete {
        WorkerRegistrationHistoryAdmission::IncompleteHistory
    } else {
        WorkerRegistrationHistoryAdmission::Fresh
    }
}

/// Whether a durable Worker slot may mint a newer incarnation/generation.
/// Quiesced and dead records are tombstones that permit replacement; a live
/// starting/ready/draining incarnation retains the slot until exact expiry.
#[must_use]
pub const fn worker_slot_is_replaceable(
    state: WorkerState,
    expires_at_ms: u64,
    now_ms: u64,
) -> bool {
    matches!(state, WorkerState::Quiesced | WorkerState::Dead) || expires_at_ms <= now_ms
}

/// Exact targeted retirement uses the same half-open lifetime as replacement
/// and heartbeat. Applied closes execution authority, never physical execution.
#[must_use]
pub const fn worker_expiry_admission(
    record_present: bool,
    identity_matches: bool,
    state: WorkerState,
    expires_at_ms: u64,
    now_ms: u64,
) -> RegistryMutation {
    if !record_present {
        RegistryMutation::NotFound
    } else if !identity_matches {
        RegistryMutation::StaleIncarnation
    } else if worker_slot_is_replaceable(state, expires_at_ms, now_ms) {
        RegistryMutation::Applied
    } else {
        RegistryMutation::InvalidTransition
    }
}

/// Closed admission table for a Worker heartbeat read from one atomic registry
/// snapshot. `Applied` is the sole result that permits a record rewrite.
#[must_use]
pub const fn worker_heartbeat_admission(
    record_present: bool,
    identity_matches: bool,
    lease_is_live: bool,
    sequence_is_newer: bool,
    state: WorkerState,
) -> RegistryMutation {
    if !record_present {
        RegistryMutation::NotFound
    } else if !identity_matches || !lease_is_live {
        RegistryMutation::StaleIncarnation
    } else if !sequence_is_newer {
        RegistryMutation::StaleSequence
    } else if matches!(state, WorkerState::Quiesced | WorkerState::Dead) {
        RegistryMutation::InvalidTransition
    } else {
        RegistryMutation::Applied
    }
}

/// Project the observation fence of one already-admitted heartbeat.
///
/// Unchanged evidence keeps its prior fence so ordinary heartbeats coalesce.
/// Changed evidence must carry a strictly newer heartbeat sequence; rejecting
/// any non-increase makes a corrupt or downgraded durable record fail closed.
#[must_use]
pub const fn worker_observation_sequence_after_heartbeat(
    current_observation_sequence: u64,
    heartbeat_sequence: u64,
    observations_changed: bool,
) -> Option<u64> {
    if !observations_changed {
        Some(current_observation_sequence)
    } else if heartbeat_sequence > current_observation_sequence {
        Some(heartbeat_sequence)
    } else {
        None
    }
}

/// Admission result for one Worker-observation fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerObservationPublicationAdmission {
    Coalesce,
    Publish,
}

/// Completion result for one Worker-observation publication attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerObservationPublicationCompletion {
    Advanced,
    Retained,
}

/// The sole successful-publication fence for Worker-observation reconciliation.
///
/// The stored fingerprint is private: callers can create an empty fence and can
/// advance it only through a successful completion. Therefore `Coalesce` always
/// names an exact fingerprint captured by a prior successful reconciliation;
/// the reconciler may have published that capture or a later source snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerObservationPublicationFence<Fingerprint> {
    last_successful: Option<Fingerprint>,
}

impl<Fingerprint> Default for WorkerObservationPublicationFence<Fingerprint> {
    fn default() -> Self {
        Self {
            last_successful: None,
        }
    }
}

impl<Fingerprint: PartialEq> WorkerObservationPublicationFence<Fingerprint> {
    /// Admit publication unless this exact fingerprint already owns the fence.
    #[must_use]
    pub fn observe(&self, fingerprint: &Fingerprint) -> WorkerObservationPublicationAdmission {
        if self.last_successful.as_ref() == Some(fingerprint) {
            WorkerObservationPublicationAdmission::Coalesce
        } else {
            WorkerObservationPublicationAdmission::Publish
        }
    }
}

impl<Fingerprint> WorkerObservationPublicationFence<Fingerprint> {
    /// Complete a publication attempt, retaining the old fence on failure.
    pub fn complete(
        &mut self,
        fingerprint: Fingerprint,
        succeeded: bool,
    ) -> WorkerObservationPublicationCompletion {
        if succeeded {
            self.last_successful = Some(fingerprint);
            WorkerObservationPublicationCompletion::Advanced
        } else {
            WorkerObservationPublicationCompletion::Retained
        }
    }
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("worker id and incarnation id must be non-empty")]
    InvalidIdentity,
    #[error("worker slot {worker_id} is occupied by generation {generation}")]
    SlotOccupied { worker_id: String, generation: u64 },
    #[error("worker slot {worker_id} cannot register a historical incarnation again")]
    HistoricalIncarnation { worker_id: String },
    #[error("worker slot {worker_id} has incomplete incarnation history; use a new worker id")]
    IncompleteIncarnationHistory { worker_id: String },
    #[error("an incarnation cannot change its registered manifest")]
    ManifestChanged,
    #[error("worker registry persistence failed: {0}")]
    Persistence(String),
    #[error("worker generation authority is exhausted for slot {worker_id}")]
    GenerationExhausted { worker_id: String },
    #[error(
        "worker process exit receipt requires non-empty provider, execution and evidence references"
    )]
    InvalidProcessExit,
    #[error("worker Create ledger requires complete exact provider coordinates")]
    InvalidCreateLedger,
    #[error("a Worker incarnation cannot replace or backfill its Create ledger")]
    CreateLedgerChanged,
}

/// Secret-free read projection of the one Worker Registry authority.
///
/// Control-plane readers depend on this narrow port instead of receiving the
/// mutation-capable directory. A split Control process can therefore use an
/// authenticated HTTP adapter while AllInOne projects the exact local authority.
///
/// R5: roster data never grants a point-query capability. Each point-query
/// caller must supply the corresponding read role at compile time.
///
/// ```compile_fail
/// use awaken_worker_contract::{WorkerCurrentSource, WorkerObservationSource};
/// fn current(source: &dyn WorkerObservationSource) {
///     let _ = source.current("worker");
/// }
/// ```
///
/// ```compile_fail
/// use awaken_worker_contract::{WorkerIdentity, WorkerIncarnationSource, WorkerObservationSource};
/// fn incarnation(source: &dyn WorkerObservationSource, identity: &WorkerIdentity) {
///     let _ = source.incarnation(identity);
/// }
/// ```
///
/// R12: an observation role cannot close Registry admission or write receipts.
///
/// ```compile_fail
/// use awaken_worker_contract::{WorkerIdentity, WorkerObservationSource};
/// fn close(source: &dyn WorkerObservationSource, identity: &WorkerIdentity) {
///     let _ = source.deregister(identity);
/// }
/// ```
#[async_trait]
pub trait WorkerObservationSource: Send + Sync {
    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError>;
}

/// Required logical-current query over the one Worker Registry authority.
/// List-only observations do not imply this capability.
#[async_trait]
pub trait WorkerCurrentSource: WorkerObservationSource {
    async fn current(&self, worker_id: &str) -> Result<Option<RegisteredWorker>, RegistryError>;
}

/// Required exact-incarnation query over the same Worker Registry authority.
/// This role has no logical-current or mutation capability. Historical stores
/// and the authenticated exact-domain HTTP adapter implement it explicitly.
#[async_trait]
pub trait WorkerIncarnationSource: WorkerObservationSource {
    async fn incarnation(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<Option<RegisteredWorker>, RegistryError>;
}

/// Preserve the existing current-only exact-identity fallback in one place.
/// It performs one required current read and never scans the roster or invents
/// historical records. An absent/nonmatching current row stays None; a query
/// failure propagates unchanged.
pub async fn read_current_incarnation(
    source: &dyn WorkerCurrentSource,
    identity: &WorkerIdentity,
) -> Result<Option<RegisteredWorker>, RegistryError> {
    Ok(source
        .current(&identity.worker_id)
        .await?
        .filter(|record| record.snapshot.identity == *identity))
}

/// Worker-directory authority. Implementations must make every mutation atomic;
/// all incarnation records remain in this authority so historical registrations
/// cannot mint new generations. Current/list expose only the highest generation.
/// A migrated slot missing generation one cannot safely admit any different
/// incarnation: use a new Worker id, never manufacture the discarded history.
#[async_trait]
pub trait WorkerDirectory: WorkerCurrentSource + WorkerIncarnationSource {
    /// Hold the existing incarnation exclusion across a final admission and its
    /// Dispatch transaction. Provider I/O must precede this guard. Unsupported
    /// adapters fail closed rather than pretending to serialize with closure.
    async fn lock_incarnation(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<Option<WorkerIncarnationGuard>, RegistryError>;
    async fn register(
        &self,
        registration: WorkerRegistration,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<RegisteredWorker, RegistryError>;

    async fn heartbeat(
        &self,
        identity: &WorkerIdentity,
        heartbeat: WorkerHeartbeat,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<RegistryMutation, RegistryError>;

    async fn begin_drain(
        &self,
        identity: &WorkerIdentity,
        deadline_ms: u64,
    ) -> Result<RegistryMutation, RegistryError>;

    async fn mark_quiesced(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError>;

    async fn deregister(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError>;

    /// Atomically close only this expired incarnation. Already closed exact
    /// records return Applied without rewriting them; missing, live or replaced
    /// records grant no retirement. Late heartbeat/register retries cannot
    /// reopen an Applied incarnation. This is not a physical stop receipt.
    async fn expire_incarnation(
        &self,
        identity: &WorkerIdentity,
        now_ms: u64,
    ) -> Result<RegistryMutation, RegistryError>;

    /// Persist or idempotently replay the sole exact process-exit receipt for a
    /// closed incarnation. A conflicting receipt is rejected and never
    /// overwrites the first provider observation.
    async fn record_process_exit(
        &self,
        _identity: &WorkerIdentity,
        _receipt: WorkerProcessExit,
    ) -> Result<RegistryMutation, RegistryError>;
}

type IncarnationRelease =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>;
type ResourceRelease = Box<dyn FnOnce(Box<dyn std::any::Any + Send>) -> IncarnationRelease + Send>;

/// One exact Registry snapshot held under its original mutation exclusion.
/// It creates no execution authority; callers still check state, time and claim.
pub struct WorkerIncarnationGuard {
    held: Option<Box<dyn std::any::Any + Send>>,
    snapshot: WorkerSnapshot,
    release: Option<IncarnationRelease>,
    release_resource: Option<ResourceRelease>,
}

impl WorkerIncarnationGuard {
    pub fn new(held: impl Send + 'static, snapshot: WorkerSnapshot) -> Self {
        Self {
            held: Some(Box::new(held)),
            snapshot,
            release: None,
            release_resource: None,
        }
    }
    pub fn new_releasing(
        snapshot: WorkerSnapshot,
        release: impl std::future::Future<Output = Result<(), String>> + Send + 'static,
    ) -> Self {
        Self {
            held: None,
            snapshot,
            release: Some(Box::pin(release)),
            release_resource: None,
        }
    }
    /// Store adapters may transfer their original transaction into the paired
    /// Dispatch transaction. This opaque resource owns no second authority.
    pub fn new_resource<T, F, Fut>(snapshot: WorkerSnapshot, resource: T, release: F) -> Self
    where
        T: Send + 'static,
        F: FnOnce(T) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
    {
        Self {
            held: Some(Box::new(resource)),
            snapshot,
            release: None,
            release_resource: Some(Box::new(move |resource| {
                Box::pin(release(
                    *resource.downcast::<T>().expect("original resource type"),
                ))
            })),
        }
    }
    /// Take only the exact backend resource; a different adapter preserves it
    /// for normal release. The receiving store must validate backend provenance.
    pub fn take_resource<T: Send + 'static>(&mut self) -> Option<T> {
        let resource = self.held.take()?;
        match resource.downcast::<T>() {
            Ok(resource) => {
                self.release_resource = None;
                Some(*resource)
            }
            Err(resource) => {
                self.held = Some(resource);
                None
            }
        }
    }
    pub fn snapshot(&self) -> &WorkerSnapshot {
        &self.snapshot
    }
    pub async fn release(mut self) -> Result<(), String> {
        if let Some(release) = self.release_resource.take()
            && let Some(resource) = self.held.take()
        {
            release(resource).await?;
        }
        if let Some(release) = self.release {
            release.await?;
        }
        drop(self.held);
        Ok(())
    }
}

impl WorkerSnapshot {
    #[must_use]
    pub fn accepts(&self, requirements: &PlacementRequirements, now_ms: u64) -> bool {
        let process_and_static_eligible = self.state.accepts_work()
            && self.expires_at_ms > now_ms
            && self.in_flight < self.manifest.capacity.max_concurrent
            && self.manifest.fingerprint().ok().as_deref()
                == Some(self.capability_fingerprint.as_str())
            && can_claim(&self.manifest, requirements).is_ok();
        let credential_evidence_satisfied =
            requirements.required_credentials.iter().all(|required| {
                self.credential_observations
                    .iter()
                    .any(|observation| observation.is_selectable_at(required, now_ms))
            });
        let acp_evidence_satisfied =
            requirements
                .required_acp_capabilities
                .iter()
                .all(|required| {
                    self.acp_capability_observations
                        .iter()
                        .any(|observation| observation.is_selectable_at(required, now_ms))
                });
        dynamic_evidence_admits(
            process_and_static_eligible,
            credential_evidence_satisfied,
            acp_evidence_satisfied,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_registration_wire_cannot_assert_a_create_ledger() {
        // Causes: C1 Coordinator has an exact non-secret ledger anchor; C2 a
        // Worker supplies a forged field in JSON; C3 any provider coordinate
        // is blank or padded. Effects: E1 the wire omits C1, E2 decoding C2
        // still yields no anchor, E3 C3 cannot be constructed. Only the
        // trusted registration adapter may fill this field before Registry CAS.
        let ledger = WorkerCreateLedger::new(
            "kubernetes",
            "awaken-control",
            "ledger",
            "uid-a",
            "config-a",
        )
        .unwrap();
        let registration = WorkerRegistration {
            worker_id: "worker".into(),
            incarnation_id: "boot".into(),
            manifest: WorkerManifest::default(),
            create_ledger: Some(ledger.clone()),
        };
        let mut wire = serde_json::to_value(&registration).unwrap();
        assert!(wire.get("create_ledger").is_none(), "E1");
        wire["create_ledger"] = serde_json::to_value(&ledger).unwrap();
        let decoded: WorkerRegistration = serde_json::from_value(wire).unwrap();
        assert!(decoded.create_ledger.is_none(), "E2");
        assert!(
            WorkerCreateLedger::new("kubernetes", "awaken-control", "ledger", " ", "config-a")
                .is_err(),
            "E3"
        );
    }

    #[test]
    fn exact_worker_lease_owner_round_trips_across_generation_and_colons() {
        // Causes: C1 full Worker id has Kubernetes colons; C2 generation is
        // positive/canonical; C3 boot suffix is exact. Only C1+C2+C3 yields
        // the historical identity used to read its Registry ledger; changing
        // any coordinate cannot produce that identity or a current fallback.
        let worker = WorkerIdentity::new("k8s:ns:pod:uid", "uid", 7);
        assert_eq!(
            WorkerIdentity::from_lease_owner(&worker.worker_id, &worker.lease_owner()),
            Some(worker.clone())
        );
        for foreign in [
            "k8s:ns:pod:uid:0:uid",
            "k8s:ns:pod:uid:07:uid",
            "k8s:ns:pod:uid:7:",
            "k8s:ns:pod:uid:7:other",
            "other:7:uid",
        ] {
            let parsed = WorkerIdentity::from_lease_owner(&worker.worker_id, foreign);
            assert!(parsed.as_ref() != Some(&worker), "foreign {foreign}");
        }
    }

    #[test]
    fn local_process_incarnation_format_is_one_shared_launch_contract() {
        // Causes: C1 exactly 32 bytes; C2 every byte is lowercase hexadecimal.
        // Effect E1 admits only C1+C2, so a launcher and the physical evidence
        // source derive the same unit coordinate; E2 rejects every other row.
        // | Rule | C1 | C2 | Effect |
        // | L1   | Y  | Y  | E1     |
        // | L2   | N  | *  | E2     |
        // | L3   | Y  | N  | E2     |
        assert!(is_local_process_incarnation(
            "0123456789abcdef0123456789abcdef"
        ));
        assert!(!is_local_process_incarnation(""));
        assert!(!is_local_process_incarnation(
            "0123456789abcdef0123456789abcde"
        ));
        assert!(!is_local_process_incarnation(
            "0123456789abcdef0123456789abcdef0"
        ));
        assert!(!is_local_process_incarnation(
            "0123456789abcdef0123456789abcdeF"
        ));
        assert!(!is_local_process_incarnation(
            "0123456789abcdef0123456789abcde/"
        ));
    }

    #[test]
    fn process_exit_receipt_admission_is_closed_exact_and_immutable() {
        // Causes: C1 record present; C2 identity exact; C3 Worker closed; C4
        // existing receipt absent/equal/conflicting. Effects: E1 only a closed
        // exact absent/equal receipt applies; E2 missing is NotFound; E3 foreign
        // identity is stale; E4 open or conflicting history is invalid.
        // Decision rules exhaust all 32 Boolean rows and assert the same formula
        // consumed by every Registry adapter.
        for present in [false, true] {
            for exact in [false, true] {
                for closed in [false, true] {
                    for existing in [false, true] {
                        for equal in [false, true] {
                            let expected = if !present {
                                RegistryMutation::NotFound
                            } else if !exact {
                                RegistryMutation::StaleIncarnation
                            } else if !closed || (existing && !equal) {
                                RegistryMutation::InvalidTransition
                            } else {
                                RegistryMutation::Applied
                            };
                            assert_eq!(
                                worker_process_exit_admission(
                                    present, exact, closed, existing, equal
                                ),
                                expected
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn startup_readiness_is_independent_of_dynamic_probe_outcome() {
        // Causes: C1 registration and runtime assembly already succeeded; C2
        // the asynchronous dynamic-evidence probe is Pending, Succeeded, or
        // Failed. Effects: E1 process readiness remains true for every C2;
        // E2 dynamic evidence is still evaluated separately by placement.
        // Constraint: this policy cannot manufacture credential/capability
        // evidence. Decision rules P1/P2/P3 = C1 + each C2 => E1, while E2 is
        // owned by `dynamic_evidence_admits`.
        for probe in [
            DynamicEvidenceProbeState::Pending,
            DynamicEvidenceProbeState::Succeeded,
            DynamicEvidenceProbeState::Failed,
        ] {
            assert!(process_ready_after_startup(probe));
        }
    }

    #[test]
    fn replacement_and_heartbeat_follow_one_non_resurrection_table() {
        // Causes: C1 record exists; C2 identity matches; C3 half-open lease is
        // live; C4 sequence is newer; C5 state is open vs Quiesced/Dead.
        // Effects: E1 only all-positive open facts apply; E2 missing is not
        // found; E3 identity/expiry is stale; E4 old sequence stutters; E5 a
        // terminal registry state is invalid; E6 every replaceable record is
        // simultaneously non-heartbeatable.
        //
        // | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
        // |---|---|---|---|---|---|---|
        // | R1 | yes | yes | yes | yes | open | E1 applied |
        // | R2 | no | any | any | any | any | E2 not found |
        // | R3 | yes | no/expired | any | any | any | E3 stale |
        // | R4 | yes | yes | yes | no | open | E4 stale sequence |
        // | R5 | yes | yes | yes | yes | terminal | E5 invalid |
        assert_eq!(
            worker_heartbeat_admission(true, true, true, true, WorkerState::Ready),
            RegistryMutation::Applied,
            "R1/E1"
        );
        assert_eq!(
            worker_heartbeat_admission(false, false, false, false, WorkerState::Dead),
            RegistryMutation::NotFound,
            "R2/E2"
        );
        assert_eq!(
            worker_heartbeat_admission(true, false, true, true, WorkerState::Ready),
            RegistryMutation::StaleIncarnation,
            "R3/E3 identity"
        );
        assert_eq!(
            worker_heartbeat_admission(true, true, false, true, WorkerState::Ready),
            RegistryMutation::StaleIncarnation,
            "R3/E3 expiry"
        );
        assert_eq!(
            worker_heartbeat_admission(true, true, true, false, WorkerState::Ready),
            RegistryMutation::StaleSequence,
            "R4/E4"
        );
        assert_eq!(
            worker_heartbeat_admission(true, true, true, true, WorkerState::Dead),
            RegistryMutation::InvalidTransition,
            "R5/E5"
        );
        for state in [
            WorkerState::Starting,
            WorkerState::Ready,
            WorkerState::Draining,
        ] {
            assert!(worker_slot_is_replaceable(state, 10, 10), "E6 exact expiry");
            assert_ne!(
                worker_heartbeat_admission(true, true, false, true, state),
                RegistryMutation::Applied,
                "E6"
            );
        }
    }

    #[test]
    fn observation_sequence_changes_only_to_a_strictly_new_fence() {
        // Causes: C1 credential/ACP evidence changed; C2 the heartbeat sequence
        // is strictly newer than the current observation fence. Effects: E1
        // unchanged evidence preserves the fence; E2 C1+C2 installs the new
        // sequence; E3 C1+!C2 rejects. Decision rules O1=!C1=>E1,
        // O2=C1+C2=>E2, and O3=C1+!C2=>E3.
        assert_eq!(
            worker_observation_sequence_after_heartbeat(7, 8, false),
            Some(7),
            "O1/E1"
        );
        assert_eq!(
            worker_observation_sequence_after_heartbeat(7, 8, true),
            Some(8),
            "O2/E2"
        );
        assert_eq!(
            worker_observation_sequence_after_heartbeat(7, 7, true),
            None,
            "O3/E3 equal"
        );
        assert_eq!(
            worker_observation_sequence_after_heartbeat(7, 6, true),
            None,
            "O3/E3 older"
        );
    }

    #[test]
    fn observation_publication_fence_is_success_owned_exact_and_failure_stable() {
        // Causes: C1 a successful fingerprint exists; C2 the observation
        // matches it exactly; C3 the current publication succeeds. Effects: E1
        // C1+C2 coalesces, E2 !C1|!C2 publishes, E3 C3 advances to the current
        // fingerprint, E4 !C3 retains the old fence. Decision rules:
        // R1=!C1=>E2; R2=C1+C2=>E1; R3=C1+!C2=>E2;
        // R4=C3=>E3; R5=!C3=>E4 with the prior admission unchanged.
        let mut fence = WorkerObservationPublicationFence::default();
        assert_eq!(
            fence.observe(&"a"),
            WorkerObservationPublicationAdmission::Publish,
            "R1/E2"
        );
        assert_eq!(
            fence.complete("a", false),
            WorkerObservationPublicationCompletion::Retained,
            "R5/E4"
        );
        assert_eq!(
            fence.observe(&"a"),
            WorkerObservationPublicationAdmission::Publish,
            "R5 retains R1"
        );
        assert_eq!(
            fence.complete("a", true),
            WorkerObservationPublicationCompletion::Advanced,
            "R4/E3"
        );
        assert_eq!(
            fence.observe(&"a"),
            WorkerObservationPublicationAdmission::Coalesce,
            "R2/E1"
        );
        assert_eq!(
            fence.observe(&"b"),
            WorkerObservationPublicationAdmission::Publish,
            "R3/E2"
        );
    }
}

#[cfg(kani)]
/// @obligation worker.drain_and_replacement_races_preserve_registry_authority
/// @obligation worker.incarnation_generation_fences_stale_processes
/// @obligation worker_credential.stale_heartbeat_never_rolls_back_revision
#[kani::proof]
fn worker_replacement_and_heartbeat_share_one_non_resurrection_boundary() {
    let state = match kani::any::<u8>() % 5 {
        0 => WorkerState::Starting,
        1 => WorkerState::Ready,
        2 => WorkerState::Draining,
        3 => WorkerState::Quiesced,
        _ => WorkerState::Dead,
    };
    let expires_at_ms = kani::any::<u64>();
    let now_ms = kani::any::<u64>();
    let identity_matches = kani::any::<bool>();
    let sequence_is_newer = kani::any::<bool>();
    let live = expires_at_ms > now_ms;
    let admission =
        worker_heartbeat_admission(true, identity_matches, live, sequence_is_newer, state);

    assert_eq!(
        worker_slot_is_replaceable(state, expires_at_ms, now_ms),
        matches!(state, WorkerState::Quiesced | WorkerState::Dead) || !live
    );
    assert_eq!(
        admission == RegistryMutation::Applied,
        identity_matches
            && live
            && sequence_is_newer
            && !matches!(state, WorkerState::Quiesced | WorkerState::Dead)
    );
    if worker_slot_is_replaceable(state, expires_at_ms, now_ms) {
        assert_ne!(admission, RegistryMutation::Applied);
    }
    // E1 missing => NotFound; E2 mismatched => StaleIncarnation; E3 exact
    // closed/expired => Applied; E4 exact live => InvalidTransition. No state
    // admits both a targeted expiry and heartbeat at the same linearization
    // snapshot, including equality and every u64 deadline. Once persisted as
    // Dead, even a delayed heartbeat carrying an older time cannot reopen it.
    let present: bool = kani::any();
    let expiry = worker_expiry_admission(present, identity_matches, state, expires_at_ms, now_ms);
    assert_eq!(expiry == RegistryMutation::NotFound, !present);
    assert_eq!(
        expiry == RegistryMutation::StaleIncarnation,
        present && !identity_matches
    );
    assert_eq!(
        expiry == RegistryMutation::Applied,
        present
            && identity_matches
            && (matches!(state, WorkerState::Quiesced | WorkerState::Dead) || !live)
    );
    if expiry == RegistryMutation::Applied {
        assert_ne!(admission, RegistryMutation::Applied);
        assert_ne!(
            worker_heartbeat_admission(true, true, true, true, WorkerState::Dead),
            RegistryMutation::Applied
        );
    }
}

#[cfg(kani)]
/// @obligation sandbox.k8s_process_observation_requires_complete_exit
#[kani::proof]
fn worker_process_exit_receipt_requires_closed_exact_immutable_history() {
    let present = kani::any::<bool>();
    let exact = kani::any::<bool>();
    let closed = kani::any::<bool>();
    let existing = kani::any::<bool>();
    let equal = kani::any::<bool>();
    let outcome = worker_process_exit_admission(present, exact, closed, existing, equal);
    assert_eq!(outcome == RegistryMutation::NotFound, !present);
    assert_eq!(
        outcome == RegistryMutation::StaleIncarnation,
        present && !exact
    );
    assert_eq!(
        outcome == RegistryMutation::Applied,
        present && exact && closed && (!existing || equal)
    );
    if existing && !equal {
        assert_ne!(outcome, RegistryMutation::Applied);
    }
}

#[cfg(kani)]
/// @obligation observation_reconcile.evidence_aba_has_fresh_fence
/// @obligation observation_reconcile.skipped_epoch_was_published_or_superseded
#[kani::proof]
fn changed_worker_observation_always_mints_a_strictly_fresh_fence() {
    let current = kani::any::<u64>();
    let heartbeat = kani::any::<u64>();
    let changed = kani::any::<bool>();
    let result = worker_observation_sequence_after_heartbeat(current, heartbeat, changed);

    assert_eq!(
        result,
        if changed {
            (heartbeat > current).then_some(heartbeat)
        } else {
            Some(current)
        }
    );
    if changed {
        assert_eq!(result.is_some(), heartbeat > current);
        if let Some(next) = result {
            assert!(next > current);
        }
    } else {
        assert_eq!(result, Some(current));
    }
}

#[cfg(kani)]
/// @obligation observation_reconcile.evidence_aba_has_fresh_fence
/// @obligation observation_reconcile.failed_refresh_does_not_advance_fence
/// @obligation observation_reconcile.skipped_epoch_was_published_or_superseded
/// @obligation observation_reconcile.unchanged_evidence_coalesces
#[kani::proof]
fn observation_publication_fence_is_success_owned_exact_and_failure_stable() {
    // Symbolic decision table over every u8 prior/current fingerprint and both
    // completion outcomes. An opaque fence starts empty; therefore Coalesce is
    // reachable exactly after a successful completion of the same fingerprint.
    let prior = kani::any::<u8>();
    let observed = kani::any::<u8>();
    let prior_succeeded = kani::any::<bool>();
    let mut fence = WorkerObservationPublicationFence::default();
    assert_eq!(
        fence.complete(prior, prior_succeeded),
        if prior_succeeded {
            WorkerObservationPublicationCompletion::Advanced
        } else {
            WorkerObservationPublicationCompletion::Retained
        }
    );
    assert_eq!(
        fence.observe(&observed),
        if prior_succeeded && observed == prior {
            WorkerObservationPublicationAdmission::Coalesce
        } else {
            WorkerObservationPublicationAdmission::Publish
        }
    );

    let before_failure = fence.observe(&observed);
    assert_eq!(
        fence.complete(kani::any::<u8>(), false),
        WorkerObservationPublicationCompletion::Retained
    );
    assert_eq!(fence.observe(&observed), before_failure);

    assert_eq!(
        fence.complete(observed, true),
        WorkerObservationPublicationCompletion::Advanced
    );
    assert_eq!(
        fence.observe(&observed),
        WorkerObservationPublicationAdmission::Coalesce
    );
}

#[cfg(test)]
#[test]
fn registration_history_decision_table() {
    // H1 unseen + complete -> fresh; H2 unseen + incomplete -> reject upgrade;
    // H3/H4 seen + either completeness -> reject historical replay. No clock or
    // current state can override these results; current retries are separately
    // exercised through the production register path by Registry conformance.
    use WorkerRegistrationHistoryAdmission::*;
    for (seen, complete, expected) in [
        (false, true, Fresh),
        (false, false, IncompleteHistory),
        (true, true, HistoricalIncarnation),
        (true, false, HistoricalIncarnation),
    ] {
        assert_eq!(
            worker_registration_history_admission(seen, complete),
            expected
        );
    }
}

#[cfg(kani)]
/// @obligation worker.incarnation_generation_fences_stale_processes
#[kani::proof]
fn worker_registration_never_grants_authority_to_seen_or_unknown_history() {
    // Scalar production decision only. Storage completeness, transaction
    // serialization, identity strings and HTTP authentication require the
    // Registry conformance/migration/transport tests; no whole-chain claim.
    let seen: bool = kani::any();
    let complete: bool = kani::any();
    let decision = worker_registration_history_admission(seen, complete);
    assert_eq!(
        matches!(decision, WorkerRegistrationHistoryAdmission::Fresh),
        !seen && complete
    );
    if seen {
        assert_eq!(
            decision,
            WorkerRegistrationHistoryAdmission::HistoricalIncarnation
        );
    }
    if !seen && !complete {
        assert_eq!(
            decision,
            WorkerRegistrationHistoryAdmission::IncompleteHistory
        );
    }
}
