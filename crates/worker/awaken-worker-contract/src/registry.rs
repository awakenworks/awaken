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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerRegistration {
    pub worker_id: String,
    pub incarnation_id: String,
    pub manifest: WorkerManifest,
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

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("worker id and incarnation id must be non-empty")]
    InvalidIdentity,
    #[error("worker slot {worker_id} is occupied by generation {generation}")]
    SlotOccupied { worker_id: String, generation: u64 },
    #[error("an incarnation cannot change its registered manifest")]
    ManifestChanged,
    #[error("worker registry persistence failed: {0}")]
    Persistence(String),
    #[error("worker generation authority is exhausted for slot {worker_id}")]
    GenerationExhausted { worker_id: String },
}

/// Secret-free read projection of the current Worker authority.
///
/// Control-plane readers depend on this narrow port instead of receiving the
/// mutation-capable directory. A split Control process can therefore use an
/// authenticated HTTP adapter while AllInOne projects the exact local authority.
#[async_trait]
pub trait WorkerObservationSource: Send + Sync {
    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError>;
}

/// Worker-directory authority. Implementations must make every mutation atomic;
/// expired/dead records remain tombstones so late messages cannot resurrect them.
#[async_trait]
pub trait WorkerDirectory: WorkerObservationSource {
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

    async fn current(&self, worker_id: &str) -> Result<Option<RegisteredWorker>, RegistryError>;

    async fn expire(&self, now_ms: u64) -> Result<Vec<WorkerIdentity>, RegistryError>;
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
}

#[cfg(kani)]
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
}
