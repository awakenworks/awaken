//! Neutral worker identity, compatibility, and placement-policy contract.
//!
//! This crate deliberately separates the non-replaceable eligibility kernel from
//! replaceable ranking policy. A policy may order workers that already satisfy the
//! durable requirements, but it cannot widen isolation, capability, version, or
//! recovery authority. Registry persistence, authentication, capacity reservation,
//! and executor channels live in server/composition crates.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use async_trait::async_trait;
use awaken_acp_contract::{AcpCapabilityObservation, AcpCapabilityObservationState};
pub use awaken_credential_contract::{
    CredentialObservationState as WorkerCredentialState, CredentialRef as WorkerCredentialRevision,
};
use awaken_provisioning_contract::{
    IsolationClass, ResourceLimits, ResourceRequests, SandboxCapabilities, SandboxCapacityShapeId,
    SandboxRequirements,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Worker can install a frozen Workspace-scoped Session resource manifest over
/// shared File/Memory/Skill/lifecycle and Resource Catalog ports.
pub const SESSION_RESOURCES_CAPABILITY: &str = "session-resources/v1";

/// Worker has an explicitly installed in-process executor for a published Host
/// candidate. This is a realization capability, not a snapshot wire scheme.
pub const HOST_EXECUTOR_CAPABILITY: &str = "host-executor/v1";

/// Worker can open the exact persisted credential source frozen into a published
/// Provider candidate. This capability grants no credential by itself.
pub const PROVIDER_CREDENTIAL_SOURCE_CAPABILITY: &str = "credential-source/v1";

/// Worker can inject a frozen Repository config's opaque credential reference at
/// realization time. Kept separate so a secretless worker remains eligible for
/// File/Memory/Skill and public Repository inputs.
pub const REPOSITORY_CREDENTIALS_CAPABILITY: &str = "repository-credentials/v1";

/// Placement-context key for a soft, exact Environment capacity preference.
pub const PREFERRED_ENVIRONMENT_SHAPE_ATTRIBUTE: &str = "environment_shape";

/// Worker can revalidate and use exact private credential revisions that never
/// cross the control plane. Use may be secret materialization or a local backend
/// (such as a CLI) reading its own login. Eligibility additionally requires a
/// current observation for every pinned revision, so this capability alone
/// grants no access.
pub const WORKER_LOCAL_CREDENTIALS_CAPABILITY: &str = "worker-local-credentials/v1";

/// Point-in-time, non-secret credential evidence published by one Worker.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WorkerCredentialObservation {
    pub credential: WorkerCredentialRevision,
    pub state: WorkerCredentialState,
    pub observed_at_ms: u64,
    /// Exclusive deadline after which this observation is no longer placement
    /// evidence. A missing field from an older sender decodes to zero and
    /// therefore fails closed.
    #[serde(default)]
    pub valid_until_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
}

impl WorkerCredentialObservation {
    #[must_use]
    pub fn available(
        credential: WorkerCredentialRevision,
        observed_at_ms: u64,
        valid_until_ms: u64,
    ) -> Self {
        Self {
            credential,
            state: WorkerCredentialState::Available,
            observed_at_ms,
            valid_until_ms,
            reason_code: None,
        }
    }

    #[must_use]
    pub fn is_selectable_at(&self, credential: &WorkerCredentialRevision, now_ms: u64) -> bool {
        self.state == WorkerCredentialState::Available
            && &self.credential == credential
            && self.observed_at_ms <= now_ms
            && now_ms < self.valid_until_ms
    }
}

/// Worker-leased wrapper around one ACP capability observation. The inner
/// profile is protocol-neutral and secret-free; expiry is Worker authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerAcpCapabilityObservation {
    pub observation: AcpCapabilityObservation,
    #[serde(default)]
    pub valid_until_ms: u64,
}

impl WorkerAcpCapabilityObservation {
    #[must_use]
    pub fn is_selectable_at(
        &self,
        requirement: &WorkerAcpCapabilityRequirement,
        now_ms: u64,
    ) -> bool {
        self.observation.is_coherent()
            && self.observation.state == AcpCapabilityObservationState::Verified
            && self.observation.backend_ref == requirement.backend_ref
            && self.observation.fingerprint.as_deref() == Some(requirement.fingerprint.as_str())
            && self.observation.observed_at_ms <= now_ms
            && now_ms < self.valid_until_ms
    }
}

/// Exact dynamic ACP profile required by an immutable publication.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WorkerAcpCapabilityRequirement {
    pub backend_ref: String,
    pub fingerprint: String,
}

pub const CURRENT_CONTRACT_VERSION: u32 = 1;

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

/// Inclusive protocol-version range supported by a worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionRange {
    pub min: u32,
    pub max: u32,
}

impl VersionRange {
    pub const ANY: Self = Self {
        min: 0,
        max: u32::MAX,
    };

    #[must_use]
    pub const fn exact(version: u32) -> Self {
        Self {
            min: version,
            max: version,
        }
    }

    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.min <= self.max
    }

    #[must_use]
    pub const fn contains(self, version: u32) -> bool {
        self.is_valid() && version >= self.min && version <= self.max
    }
}

impl Default for VersionRange {
    fn default() -> Self {
        Self::ANY
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerCapacity {
    pub max_concurrent: u32,
    /// Optional maximum resource request one sandbox assigned to this Worker may
    /// make. Entirely unset delegates feasibility to the sandbox backend (for
    /// example Kubernetes); it is never aggregate inventory or billing data.
    #[serde(default)]
    pub resources: ResourceLimits,
}

impl Default for WorkerCapacity {
    fn default() -> Self {
        Self {
            max_concurrent: 1,
            resources: ResourceLimits::default(),
        }
    }
}

/// Immutable capabilities for one worker incarnation. Dynamic health/load does
/// not belong here and therefore cannot perturb the capability fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerManifest {
    pub manifest_version: u32,
    pub build_digest: String,
    #[serde(default)]
    pub capabilities: BTreeSet<String>,
    pub zone: Option<String>,
    pub architecture: String,
    pub sandbox: SandboxCapabilities,
    /// Trusted recovery capability of the SessionEnvironment-owned Sandbox
    /// executor. Production derives this from the same typed deployment value
    /// that constructs the executor; it is never an Agent-authored claim.
    #[serde(default)]
    pub sandbox_tool_recovery: awaken_runtime_contract::tool::ToolRecoveryCapability,
    #[serde(default)]
    pub sandbox_backends: BTreeSet<String>,
    #[serde(default)]
    pub dispatch_contract: VersionRange,
    #[serde(default)]
    pub runtime_protocol: VersionRange,
    #[serde(default)]
    pub checkpoint_formats: BTreeSet<String>,
    #[serde(default)]
    pub capacity: WorkerCapacity,
}

impl Default for WorkerManifest {
    fn default() -> Self {
        Self {
            manifest_version: CURRENT_CONTRACT_VERSION,
            build_digest: String::new(),
            capabilities: BTreeSet::new(),
            zone: None,
            architecture: std::env::consts::ARCH.to_string(),
            sandbox: SandboxCapabilities {
                isolation: IsolationClass::Workdir,
                tool_transparent: false,
                path_fidelity: false,
                enforced_readonly: false,
                network_isolation: false,
                enforced_network_allowlist: false,
                secret_egress_substitution: false,
                resource_limits: false,
                custom_rootfs: false,
                package_provisioning: false,
            },
            sandbox_tool_recovery:
                awaken_runtime_contract::tool::ToolRecoveryCapability::NonRecoverable,
            sandbox_backends: BTreeSet::new(),
            dispatch_contract: VersionRange::ANY,
            runtime_protocol: VersionRange::ANY,
            checkpoint_formats: BTreeSet::new(),
            capacity: WorkerCapacity::default(),
        }
    }
}

impl WorkerManifest {
    /// Content address of immutable capabilities. BTree collections and struct
    /// field order make the JSON canonical for this version of the contract.
    pub fn fingerprint(&self) -> Result<String, FingerprintError> {
        let encoded = serde_json::to_vec(self).map_err(FingerprintError::Serialize)?;
        Ok(format!("sha256:{:x}", Sha256::digest(encoded)))
    }
}

#[derive(Debug, Error)]
pub enum FingerprintError {
    #[error("worker manifest cannot be serialized: {0}")]
    Serialize(serde_json::Error),
}

/// Whether the execution location may fall back. This is distinct from sandbox
/// isolation degradation: neither policy can weaken the other's hard floor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionLocation {
    /// Backward-compatible posture for legacy rows: prefer a remote worker, while
    /// the composition root may explicitly choose its local executor.
    #[default]
    RemotePreferred,
    RemoteRequired,
    LocalOnly,
}

/// Run-level replacement behavior. Per-tool side-effect replay remains governed
/// by the existing `ToolRecoveryPolicy` pinned in the executable snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRecoveryMode {
    /// Rebuild the runtime from committed truth; existing tool policies decide
    /// whether interrupted calls replay, reconnect, or become indeterminate.
    #[default]
    RebuildFromCommittedTruth,
    /// A replacement must adopt the already-bound sandbox.
    RequireSandboxContinuity,
    /// Never automatically assign the run to another worker incarnation.
    NeverReplace,
}

/// Durable hard requirements pinned when a Run enters dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementRequirements {
    /// Zero denotes a row authored before this contract existed. It preserves the
    /// old local/remote-preferred posture, while new strict builders write v1.
    #[serde(default)]
    pub contract_version: u32,
    #[serde(default)]
    pub required_capabilities: BTreeSet<String>,
    /// Worker-private credential revisions required by the complete published
    /// candidate set. Shared-vault references do not belong here.
    #[serde(default)]
    pub required_credentials: BTreeSet<WorkerCredentialRevision>,
    /// Exact, expiring ACP profiles frozen by BackendOwned publications.
    #[serde(default)]
    pub required_acp_capabilities: BTreeSet<WorkerAcpCapabilityRequirement>,
    pub required_zone: Option<String>,
    pub architecture: Option<String>,
    #[serde(default)]
    pub sandbox: SandboxRequirements,
    /// Every non-default recovery mode frozen on a Sandbox-target tool. The
    /// selected Worker executor must support all of them before it may claim.
    #[serde(default)]
    pub required_sandbox_tool_recovery: BTreeSet<awaken_runtime_contract::tool::ToolRecoveryMode>,
    pub sandbox_backend: Option<String>,
    #[serde(default)]
    pub dispatch_contract_version: u32,
    #[serde(default)]
    pub runtime_protocol_version: u32,
    pub checkpoint_format: Option<String>,
    #[serde(default)]
    pub location: ExecutionLocation,
    #[serde(default)]
    pub recovery: WorkerRecoveryMode,
    /// Exact per-sandbox scheduling demand frozen at admission.
    #[serde(default)]
    pub resources: ResourceRequests,
}

impl Default for PlacementRequirements {
    fn default() -> Self {
        Self {
            contract_version: 0,
            required_capabilities: BTreeSet::new(),
            required_credentials: BTreeSet::new(),
            required_acp_capabilities: BTreeSet::new(),
            required_zone: None,
            architecture: None,
            sandbox: SandboxRequirements::default(),
            required_sandbox_tool_recovery: BTreeSet::new(),
            sandbox_backend: None,
            dispatch_contract_version: 0,
            runtime_protocol_version: 0,
            checkpoint_format: None,
            location: ExecutionLocation::RemotePreferred,
            recovery: WorkerRecoveryMode::RebuildFromCommittedTruth,
            resources: ResourceRequests::default(),
        }
    }
}

impl PlacementRequirements {
    /// Whether this is the exact backward-compatible posture omitted from legacy
    /// durable queue rows.
    #[must_use]
    pub fn is_legacy_default(&self) -> bool {
        self == &Self::default()
    }

    #[must_use]
    pub fn remote_required() -> Self {
        Self {
            contract_version: CURRENT_CONTRACT_VERSION,
            location: ExecutionLocation::RemoteRequired,
            dispatch_contract_version: CURRENT_CONTRACT_VERSION,
            runtime_protocol_version: CURRENT_CONTRACT_VERSION,
            ..Self::default()
        }
    }

    /// Author a current-contract request that may run locally or on a compatible
    /// registered Worker. [`Default`] remains the durable legacy decoder posture
    /// (v0), so new admission code must use this constructor instead of silently
    /// publishing an obsolete protocol requirement.
    #[must_use]
    pub fn remote_preferred() -> Self {
        Self {
            contract_version: CURRENT_CONTRACT_VERSION,
            location: ExecutionLocation::RemotePreferred,
            dispatch_contract_version: CURRENT_CONTRACT_VERSION,
            runtime_protocol_version: CURRENT_CONTRACT_VERSION,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Incompatibility {
    #[error("local-only work cannot be claimed by a remote worker")]
    LocalOnly,
    #[error("worker capacity must be greater than zero")]
    ZeroCapacity,
    #[error("worker per-sandbox resource ceiling is insufficient")]
    InsufficientResources,
    #[error("missing capability {0}")]
    MissingCapability(String),
    #[error("required zone {required}, worker zone is {actual:?}")]
    Zone {
        required: String,
        actual: Option<String>,
    },
    #[error("required architecture {required}, worker architecture is {actual}")]
    Architecture { required: String, actual: String },
    #[error("sandbox isolation or enforcement capabilities are insufficient")]
    SandboxCapabilities,
    #[error("sandbox tool recovery mode {required:?} is unsupported by {actual:?}")]
    SandboxToolRecovery {
        required: awaken_runtime_contract::tool::ToolRecoveryMode,
        actual: awaken_runtime_contract::tool::ToolRecoveryCapability,
    },
    #[error("sandbox backend {0} is unsupported")]
    SandboxBackend(String),
    #[error("dispatch contract version {0} is unsupported")]
    DispatchVersion(u32),
    #[error("runtime protocol version {0} is unsupported")]
    RuntimeVersion(u32),
    #[error("checkpoint format {0} is unsupported")]
    CheckpointFormat(String),
}

/// Non-replaceable compatibility kernel. It is intentionally independent from
/// liveness and ranking; callers re-run it inside the atomic claim transaction.
pub fn can_claim(
    manifest: &WorkerManifest,
    requirements: &PlacementRequirements,
) -> Result<(), Incompatibility> {
    if matches!(requirements.location, ExecutionLocation::LocalOnly) {
        return Err(Incompatibility::LocalOnly);
    }
    if manifest.capacity.max_concurrent == 0 {
        return Err(Incompatibility::ZeroCapacity);
    }
    if manifest.capacity.resources.is_set()
        && !requirements
            .resources
            .fits_within(&manifest.capacity.resources)
    {
        return Err(Incompatibility::InsufficientResources);
    }
    if let Some(missing) = requirements
        .required_capabilities
        .iter()
        .find(|capability| !manifest.capabilities.contains(*capability))
    {
        return Err(Incompatibility::MissingCapability(missing.clone()));
    }
    if let Some(required) = &requirements.required_zone
        && manifest.zone.as_ref() != Some(required)
    {
        return Err(Incompatibility::Zone {
            required: required.clone(),
            actual: manifest.zone.clone(),
        });
    }
    if let Some(required) = &requirements.architecture
        && &manifest.architecture != required
    {
        return Err(Incompatibility::Architecture {
            required: required.clone(),
            actual: manifest.architecture.clone(),
        });
    }
    if !manifest
        .sandbox
        .satisfies_requirements(&requirements.sandbox)
    {
        return Err(Incompatibility::SandboxCapabilities);
    }
    if let Some(required) = requirements
        .required_sandbox_tool_recovery
        .iter()
        .find(|required| !required.is_supported_by(manifest.sandbox_tool_recovery))
    {
        return Err(Incompatibility::SandboxToolRecovery {
            required: *required,
            actual: manifest.sandbox_tool_recovery,
        });
    }
    if let Some(backend) = &requirements.sandbox_backend
        && !manifest.sandbox_backends.contains(backend)
    {
        return Err(Incompatibility::SandboxBackend(backend.clone()));
    }
    if !manifest
        .dispatch_contract
        .contains(requirements.dispatch_contract_version)
    {
        return Err(Incompatibility::DispatchVersion(
            requirements.dispatch_contract_version,
        ));
    }
    if !manifest
        .runtime_protocol
        .contains(requirements.runtime_protocol_version)
    {
        return Err(Incompatibility::RuntimeVersion(
            requirements.runtime_protocol_version,
        ));
    }
    if let Some(format) = &requirements.checkpoint_format
        && !manifest.checkpoint_formats.contains(format)
    {
        return Err(Incompatibility::CheckpointFormat(format.clone()));
    }
    Ok(())
}

/// Whether an unregistered in-process executor may claim this run. A
/// worker-private credential requirement is remote-only even if a malformed or
/// legacy producer omitted the matching location flag.
#[must_use]
pub fn can_claim_locally(requirements: &PlacementRequirements) -> bool {
    requirements.location != ExecutionLocation::RemoteRequired
        && requirements.required_credentials.is_empty()
        && requirements.required_acp_capabilities.is_empty()
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
        self.state.accepts_work()
            && self.expires_at_ms > now_ms
            && self.in_flight < self.manifest.capacity.max_concurrent
            && self.manifest.fingerprint().ok().as_deref()
                == Some(self.capability_fingerprint.as_str())
            && can_claim(&self.manifest, requirements).is_ok()
            && requirements.required_credentials.iter().all(|required| {
                self.credential_observations
                    .iter()
                    .any(|observation| observation.is_selectable_at(required, now_ms))
            })
            && requirements
                .required_acp_capabilities
                .iter()
                .all(|required| {
                    self.acp_capability_observations
                        .iter()
                        .any(|observation| observation.is_selectable_at(required, now_ms))
                })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementContext {
    pub run_id: String,
    pub workspace_id: String,
    pub requirements: PlacementRequirements,
    pub recovered: bool,
    pub previous_worker: Option<WorkerIdentity>,
    pub attributes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankedWorker {
    pub identity: WorkerIdentity,
    pub score: i64,
    pub reason: String,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PlacementError {
    #[error("no eligible worker")]
    NoEligibleWorker,
    #[error("placement policy failed: {0}")]
    Policy(String),
    #[error("placement policy returned an ineligible worker: {0}")]
    IneligibleResult(String),
    #[error("placement policy returned a worker more than once: {0}")]
    DuplicateResult(String),
}

/// Replaceable preference only. It receives an already-filtered candidate list.
pub trait PlacementPolicy: Send + Sync {
    fn id(&self) -> &str;

    fn rank(
        &self,
        context: &PlacementContext,
        eligible: &[WorkerSnapshot],
    ) -> Result<Vec<RankedWorker>, PlacementError>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct LeastLoadedPolicy;

impl PlacementPolicy for LeastLoadedPolicy {
    fn id(&self) -> &str {
        "least-loaded"
    }

    fn rank(
        &self,
        context: &PlacementContext,
        eligible: &[WorkerSnapshot],
    ) -> Result<Vec<RankedWorker>, PlacementError> {
        let mut workers = eligible.to_vec();
        let preferred = context
            .attributes
            .get(PREFERRED_ENVIRONMENT_SHAPE_ATTRIBUTE);
        workers.sort_by(|left, right| {
            let left_warm = preferred
                .is_some_and(|shape| left.warm_environment_shapes.contains(shape.as_str()));
            let right_warm = preferred
                .is_some_and(|shape| right.warm_environment_shapes.contains(shape.as_str()));
            right_warm.cmp(&left_warm).then_with(|| {
                left.in_flight
                    .cmp(&right.in_flight)
                    .then_with(|| left.identity.cmp(&right.identity))
            })
        });
        Ok(workers
            .into_iter()
            .map(|worker| {
                let warm = preferred
                    .is_some_and(|shape| worker.warm_environment_shapes.contains(shape.as_str()));
                RankedWorker {
                    identity: worker.identity,
                    score: if warm { 1_000_000 } else { 0 } - i64::from(worker.in_flight),
                    reason: if warm {
                        "ready Environment shape, then least in-flight work"
                    } else {
                        "least in-flight work"
                    }
                    .to_string(),
                }
            })
            .collect())
    }
}

/// Filter through the immutable kernel, invoke the extension, then validate its
/// output again. A buggy or malicious extension can fail placement but cannot
/// widen authority.
pub fn place(
    policy: &dyn PlacementPolicy,
    context: &PlacementContext,
    workers: &[WorkerSnapshot],
    now_ms: u64,
) -> Result<RankedWorker, PlacementError> {
    let eligible = workers
        .iter()
        .filter(|worker| worker.accepts(&context.requirements, now_ms))
        .cloned()
        .collect::<Vec<_>>();
    rank_eligible(policy, context, eligible)
}

/// Placement for a concrete dispatch assignment. Unlike [`place`], this also
/// applies replacement and sandbox-continuity constraints from the durable
/// prior assignment. The extension still receives only eligible candidates.
pub fn place_assignment(
    policy: &dyn PlacementPolicy,
    context: &PlacementContext,
    workers: &[WorkerSnapshot],
    previous: Option<&WorkerAssignment>,
    sandbox_bound: bool,
    now_ms: u64,
) -> Result<RankedWorker, PlacementError> {
    let eligible = workers
        .iter()
        .filter(|worker| {
            can_assign(
                worker,
                &context.requirements,
                previous,
                sandbox_bound,
                now_ms,
            )
            .is_ok()
        })
        .cloned()
        .collect::<Vec<_>>();
    rank_eligible(policy, context, eligible)
}

fn rank_eligible(
    policy: &dyn PlacementPolicy,
    context: &PlacementContext,
    eligible: Vec<WorkerSnapshot>,
) -> Result<RankedWorker, PlacementError> {
    if eligible.is_empty() {
        return Err(PlacementError::NoEligibleWorker);
    }
    let allowed = eligible
        .iter()
        .map(|worker| worker.identity.clone())
        .collect::<HashSet<_>>();
    let ranked = policy.rank(context, &eligible)?;
    let mut seen = HashSet::new();
    let mut selected = None;
    for candidate in ranked {
        if !allowed.contains(&candidate.identity) {
            return Err(PlacementError::IneligibleResult(
                candidate.identity.worker_id,
            ));
        }
        if !seen.insert(candidate.identity.clone()) {
            return Err(PlacementError::DuplicateResult(
                candidate.identity.worker_id,
            ));
        }
        if selected.is_none() {
            selected = Some(candidate);
        }
    }
    selected.ok_or(PlacementError::NoEligibleWorker)
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn accepted_version_is_inside_worker_range() {
        let min = kani::any::<u32>();
        let max = kani::any::<u32>();
        let version = kani::any::<u32>();
        let range = VersionRange { min, max };
        if range.contains(version) {
            assert!(min <= max);
            assert!(version >= min);
            assert!(version <= max);
        }
    }

    #[kani::proof]
    fn non_ready_worker_never_accepts_work() {
        let state = match kani::any::<u8>() % 4 {
            0 => WorkerState::Starting,
            1 => WorkerState::Draining,
            2 => WorkerState::Quiesced,
            _ => WorkerState::Dead,
        };
        assert!(!state.accepts_work());
    }

    #[kani::proof]
    fn never_replace_rejects_every_replacement() {
        let sandbox_bound = kani::any::<bool>();
        assert!(matches!(
            assignment_recovery_rejection(true, WorkerRecoveryMode::NeverReplace, sandbox_bound,),
            Some(AssignmentRejection::ReplacementForbidden)
        ));
    }

    #[kani::proof]
    fn sandbox_continuity_authorizes_replacement_exactly_when_bound() {
        let sandbox_bound = kani::any::<bool>();
        assert_eq!(
            assignment_recovery_rejection(
                true,
                WorkerRecoveryMode::RequireSandboxContinuity,
                sandbox_bound,
            )
            .is_none(),
            sandbox_bound
        );
    }

    #[kani::proof]
    fn same_incarnation_never_spends_replacement_authority() {
        let recovery = match kani::any::<u8>() % 3 {
            0 => WorkerRecoveryMode::RebuildFromCommittedTruth,
            1 => WorkerRecoveryMode::RequireSandboxContinuity,
            _ => WorkerRecoveryMode::NeverReplace,
        };
        assert!(assignment_recovery_rejection(false, recovery, kani::any()).is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn manifest(id: &str, load: u32) -> WorkerSnapshot {
        let mut manifest = WorkerManifest {
            build_digest: id.to_string(),
            capabilities: BTreeSet::from(["mcp".to_string(), "github".to_string()]),
            zone: Some("cn-a".to_string()),
            sandbox: SandboxCapabilities {
                isolation: IsolationClass::Container,
                tool_transparent: true,
                path_fidelity: true,
                enforced_readonly: true,
                network_isolation: true,
                enforced_network_allowlist: true,
                secret_egress_substitution: true,
                resource_limits: true,
                custom_rootfs: true,
                package_provisioning: false,
            },
            sandbox_backends: BTreeSet::from(["kubernetes".to_string()]),
            dispatch_contract: VersionRange { min: 1, max: 2 },
            runtime_protocol: VersionRange { min: 1, max: 3 },
            checkpoint_formats: BTreeSet::from(["stream-v1".to_string()]),
            capacity: WorkerCapacity {
                max_concurrent: 4,
                ..WorkerCapacity::default()
            },
            ..WorkerManifest::default()
        };
        manifest.architecture = "x86_64".to_string();
        let capability_fingerprint = manifest.fingerprint().unwrap();
        WorkerSnapshot {
            identity: WorkerIdentity::new(id, format!("boot-{id}"), 1),
            state: WorkerState::Ready,
            manifest,
            capability_fingerprint,
            in_flight: load,
            warm_environment_shapes: BTreeSet::new(),
            credential_observations: BTreeSet::new(),
            acp_capability_observations: Vec::new(),
            expires_at_ms: 1_000,
        }
    }

    fn requirements() -> PlacementRequirements {
        PlacementRequirements {
            required_capabilities: BTreeSet::from(["github".to_string()]),
            required_zone: Some("cn-a".to_string()),
            architecture: Some("x86_64".to_string()),
            sandbox: SandboxRequirements {
                isolation: IsolationClass::Container,
                tool_transparent: true,
                path_fidelity: true,
                enforced_readonly: true,
                network_isolation: true,
                enforced_network_allowlist: true,
                resource_limits: true,
                custom_rootfs: true,
                package_provisioning: false,
            },
            sandbox_backend: Some("kubernetes".to_string()),
            dispatch_contract_version: 1,
            runtime_protocol_version: 2,
            checkpoint_format: Some("stream-v1".to_string()),
            ..PlacementRequirements::remote_required()
        }
    }

    fn context(requirements: PlacementRequirements) -> PlacementContext {
        PlacementContext {
            run_id: "run-1".to_string(),
            workspace_id: "ws-1".to_string(),
            requirements,
            recovered: false,
            previous_worker: None,
            attributes: BTreeMap::new(),
        }
    }

    #[test]
    fn worker_credential_evidence_reuses_the_canonical_credential_identity_and_state() {
        // Cause/effect decision table: R1 a canonical CredentialRef is accepted by
        // Worker placement without translation; R2 the canonical observation state
        // is stored unchanged. This compile-time assignment prevents a second Worker
        // identity/state representation from returning.
        let canonical = awaken_credential_contract::CredentialRef {
            id: "cred:canonical".into(),
            revision: 11,
        };
        let worker_key: WorkerCredentialRevision = canonical.clone();
        let canonical_again: awaken_credential_contract::CredentialRef = worker_key.clone();
        assert_eq!(canonical_again, canonical);

        let state = awaken_credential_contract::CredentialObservationState::Available;
        let worker_state: WorkerCredentialState = state;
        assert_eq!(worker_state, state);
        assert!(
            WorkerCredentialObservation::available(worker_key, 5, 10)
                .is_selectable_at(&canonical, 5)
        );
    }

    #[test]
    fn full_manifest_satisfies_full_requirements() {
        // Decision-table success row: every protocol, capability, topology and
        // Sandbox cause is present, therefore claim admission has no rejection
        // effect. Negative rows are partitioned by the two tests below.
        assert!(can_claim(&manifest("a", 0).manifest, &requirements()).is_ok());
    }

    #[test]
    fn sandbox_tool_recovery_is_a_hard_claim_axis() {
        use awaken_runtime_contract::tool::{ToolRecoveryCapability, ToolRecoveryMode};

        // Cause/effect decision table:
        // C1=a snapshot demands no non-default Sandbox recovery; C2=it demands
        // DurableRequest; C3=the Worker truthfully advertises DurableRequest.
        // R1 !C2 => any executor remains eligible; R2 C2+C3 => eligible; R3
        // C2+!C3 => fail closed before ranking with the exact mismatch. This
        // keeps deployment drift Pending instead of entering an incompatible
        // SessionEnvironment and discovering the mismatch during tool use.
        let mut worker = manifest("recovery-worker", 0).manifest;
        let mut required = requirements();
        assert!(can_claim(&worker, &required).is_ok(), "R1");

        required
            .required_sandbox_tool_recovery
            .insert(ToolRecoveryMode::DurableRequest);
        worker.sandbox_tool_recovery = ToolRecoveryCapability::DurableRequest;
        assert!(can_claim(&worker, &required).is_ok(), "R2");

        worker.sandbox_tool_recovery = ToolRecoveryCapability::NonRecoverable;
        assert_eq!(
            can_claim(&worker, &required),
            Err(Incompatibility::SandboxToolRecovery {
                required: ToolRecoveryMode::DurableRequest,
                actual: ToolRecoveryCapability::NonRecoverable,
            }),
            "R3"
        );
    }

    #[test]
    fn resource_demand_is_a_hard_worker_eligibility_axis() {
        // Cause/effect decision table: C1 Worker ceiling is entirely delegated;
        // C2 every demanded axis has an explicit ceiling; C3 every ceiling is
        // large enough. R1 C1 => backend decides and Worker remains eligible;
        // R2 !C1+C2+C3 => eligible; R3 !C1+!C2 and R4 !C1+C2+!C3 =>
        // InsufficientResources. Ranking is not invoked for rejected rows.
        let mut worker = manifest("resource-worker", 0).manifest;
        assert!(can_claim(&worker, &requirements()).is_ok(), "R1");

        let mut required = requirements();
        required.resources = ResourceRequests {
            cpu_millis: Some(1_000),
            memory_bytes: Some(1 << 30),
            disk_bytes: None,
        };
        assert!(can_claim(&worker, &required).is_ok(), "R1 delegated");

        worker.capacity.resources = ResourceLimits {
            cpu_millis: Some(1_000),
            memory_bytes: Some(1 << 30),
            pids: None,
            disk_bytes: None,
        };
        assert!(can_claim(&worker, &required).is_ok(), "R2");
        worker.capacity.resources.memory_bytes = None;
        assert_eq!(
            can_claim(&worker, &required),
            Err(Incompatibility::InsufficientResources),
            "R3"
        );
        worker.capacity.resources.memory_bytes = Some((1 << 30) - 1);
        assert_eq!(
            can_claim(&worker, &required),
            Err(Incompatibility::InsufficientResources),
            "R4"
        );
    }

    #[test]
    fn every_sandbox_capability_axis_fails_closed_through_one_predicate() {
        // Cause/effect graph: each SandboxRequirements bit is a conjunctive cause;
        // E1=all are satisfied -> claimable; E2=any one absent -> the single
        // SandboxCapabilities incompatibility. Isolation is ordered, boolean axes
        // are implication constraints, and unrelated Worker axes stay fixed.
        //
        // Decision table: R1 full vector -> accept (owned by the preceding test);
        // R2 weaker isolation -> reject; R3..R10 one missing enforcement bit ->
        // reject. Package provisioning is added to both sides for its positive row
        // before being removed from the Worker for its negative row.
        let required = requirements();
        let assert_rejected = |worker: WorkerManifest, rule: &str| {
            assert_eq!(
                can_claim(&worker, &required),
                Err(Incompatibility::SandboxCapabilities),
                "{rule}"
            );
        };

        let mut worker = manifest("a", 0).manifest;
        worker.sandbox.isolation = IsolationClass::Namespace;
        assert_rejected(worker, "R2 isolation");

        for axis in [
            "tool_transparent",
            "path_fidelity",
            "enforced_readonly",
            "network_isolation",
            "enforced_network_allowlist",
            "resource_limits",
            "custom_rootfs",
        ] {
            let mut worker = manifest("a", 0).manifest;
            match axis {
                "tool_transparent" => worker.sandbox.tool_transparent = false,
                "path_fidelity" => worker.sandbox.path_fidelity = false,
                "enforced_readonly" => worker.sandbox.enforced_readonly = false,
                "network_isolation" => worker.sandbox.network_isolation = false,
                "enforced_network_allowlist" => worker.sandbox.enforced_network_allowlist = false,
                "resource_limits" => worker.sandbox.resource_limits = false,
                "custom_rootfs" => worker.sandbox.custom_rootfs = false,
                _ => unreachable!(),
            }
            assert_rejected(worker, axis);
        }

        let mut package_required = required;
        package_required.sandbox.package_provisioning = true;
        let worker = manifest("a", 0).manifest;
        assert_eq!(
            can_claim(&worker, &package_required),
            Err(Incompatibility::SandboxCapabilities),
            "R10 package_provisioning"
        );
    }

    #[test]
    fn worker_private_credential_requires_the_exact_live_revision() {
        let required = WorkerCredentialRevision {
            id: "cred:worker".into(),
            revision: 7,
        };
        let mut requirements = requirements();
        requirements.required_credentials.insert(required.clone());

        let mut worker = manifest("a", 0);
        assert!(!worker.accepts(&requirements, 10));
        worker
            .credential_observations
            .insert(WorkerCredentialObservation::available(
                WorkerCredentialRevision {
                    id: required.id.clone(),
                    revision: 6,
                },
                9,
                100,
            ));
        assert!(!worker.accepts(&requirements, 10));
        worker
            .credential_observations
            .insert(WorkerCredentialObservation {
                credential: required.clone(),
                state: WorkerCredentialState::LoginRequired,
                observed_at_ms: 10,
                valid_until_ms: 100,
                reason_code: Some("credential_login_required".into()),
            });
        assert!(
            !worker.accepts(&requirements, 10),
            "an exact but unavailable state is not placement evidence"
        );
        worker
            .credential_observations
            .insert(WorkerCredentialObservation::available(required, 10, 100));
        assert!(worker.accepts(&requirements, 10));
    }

    #[test]
    fn worker_private_credential_observation_is_a_bounded_fact() {
        let required = WorkerCredentialRevision {
            id: "cred:worker".into(),
            revision: 7,
        };
        let mut requirements = requirements();
        requirements.required_credentials.insert(required.clone());
        let mut worker = manifest("a", 0);

        worker
            .credential_observations
            .insert(WorkerCredentialObservation::available(
                required.clone(),
                100,
                200,
            ));

        assert!(
            !worker.accepts(&requirements, 99),
            "future evidence is invalid"
        );
        assert!(
            worker.accepts(&requirements, 100),
            "lower bound is inclusive"
        );
        assert!(worker.accepts(&requirements, 199));
        assert!(
            !worker.accepts(&requirements, 200),
            "valid-until is an exclusive upper bound"
        );
    }

    // Cause/effect decision table for publication-pinned ACP capability:
    // A1 exact backend+fingerprint, Verified, inside TTL -> selectable.
    // A2 wrong fingerprint/backend or negative state       -> reject.
    // A3 future observation or now >= valid_until          -> reject.
    // A4 Verified state with incomplete/mixed evidence     -> reject.
    #[test]
    fn acp_capability_requires_the_exact_live_fingerprint() {
        let required = WorkerAcpCapabilityRequirement {
            backend_ref: "acp:codex".into(),
            fingerprint: "sha256:expected".into(),
        };
        let mut requirements = requirements();
        requirements
            .required_acp_capabilities
            .insert(required.clone());
        let mut worker = manifest("a", 0);
        let observation = |backend_ref: &str,
                           fingerprint: &str,
                           state: AcpCapabilityObservationState| {
            let verified = state == AcpCapabilityObservationState::Verified;
            WorkerAcpCapabilityObservation {
                observation: AcpCapabilityObservation {
                    backend_ref: backend_ref.into(),
                    adapter_version: "test".into(),
                    state,
                    observed_at_ms: 100,
                    fingerprint: verified.then(|| fingerprint.into()),
                    negotiated: verified.then(|| awaken_acp_contract::NegotiatedAcpCapabilities {
                        protocol_version: "1".into(),
                        load_session: false,
                        prompt_image: false,
                        prompt_audio: false,
                        prompt_embedded_context: false,
                        mcp_http: false,
                        mcp_sse: false,
                        session_list: false,
                        modes: Vec::new(),
                        config_options: Vec::new(),
                    }),
                    reason_code: (!verified).then(|| "not_verified".into()),
                },
                valid_until_ms: 200,
            }
        };

        worker.acp_capability_observations = vec![observation(
            "acp:codex",
            "sha256:other",
            AcpCapabilityObservationState::Verified,
        )];
        assert!(!worker.accepts(&requirements, 150), "A2");
        worker.acp_capability_observations = vec![observation(
            "acp:codex",
            "sha256:expected",
            AcpCapabilityObservationState::Unavailable,
        )];
        assert!(!worker.accepts(&requirements, 150), "A2");
        worker.acp_capability_observations = vec![observation(
            "acp:codex",
            "sha256:expected",
            AcpCapabilityObservationState::Verified,
        )];
        assert!(!worker.accepts(&requirements, 99), "A3");
        assert!(worker.accepts(&requirements, 100), "A1");
        assert!(worker.accepts(&requirements, 199), "A1");
        assert!(!worker.accepts(&requirements, 200), "A3");
        worker.acp_capability_observations[0].observation.negotiated = None;
        assert!(!worker.accepts(&requirements, 150), "A4");
    }

    #[test]
    fn one_failed_credential_observation_does_not_block_an_unrelated_requirement() {
        let required = WorkerCredentialRevision {
            id: "cred:healthy".into(),
            revision: 2,
        };
        let mut requirements = requirements();
        requirements.required_credentials.insert(required.clone());
        let mut worker = manifest("a", 0);
        worker
            .credential_observations
            .insert(WorkerCredentialObservation {
                credential: WorkerCredentialRevision {
                    id: "cred:failed".into(),
                    revision: 1,
                },
                state: WorkerCredentialState::ProbeFailed,
                observed_at_ms: 100,
                valid_until_ms: 200,
                reason_code: Some("credential_probe_failed".into()),
            });
        worker
            .credential_observations
            .insert(WorkerCredentialObservation::available(required, 100, 200));

        assert!(worker.accepts(&requirements, 150));
    }

    #[test]
    fn legacy_observation_without_a_deadline_fails_closed() {
        let observation: WorkerCredentialObservation = serde_json::from_value(serde_json::json!({
            "credential": { "id": "cred:legacy", "revision": 1 },
            "state": "available",
            "observed_at_ms": 100
        }))
        .expect("legacy wire shape remains decodable");
        assert_eq!(observation.valid_until_ms, 0);
        assert!(!observation.is_selectable_at(&observation.credential, 100));
    }

    #[test]
    fn every_hard_axis_fails_closed() {
        let worker = manifest("a", 0);
        let mut cases = Vec::new();
        let mut r = requirements();
        r.required_capabilities.insert("gpu".to_string());
        cases.push(r);
        let mut r = requirements();
        r.required_zone = Some("cn-b".to_string());
        cases.push(r);
        let mut r = requirements();
        r.architecture = Some("aarch64".to_string());
        cases.push(r);
        let mut r = requirements();
        r.dispatch_contract_version = 9;
        cases.push(r);
        let mut r = requirements();
        r.runtime_protocol_version = 9;
        cases.push(r);
        let mut r = requirements();
        r.sandbox_backend = Some("firecracker".to_string());
        cases.push(r);
        let mut r = requirements();
        r.checkpoint_format = Some("unknown".to_string());
        cases.push(r);
        let mut r = requirements();
        r.location = ExecutionLocation::LocalOnly;
        cases.push(r);
        assert!(
            cases
                .iter()
                .all(|r| can_claim(&worker.manifest, r).is_err())
        );
    }

    #[test]
    fn least_loaded_is_deterministic() {
        let workers = vec![manifest("b", 1), manifest("a", 1), manifest("c", 3)];
        let selected = place(&LeastLoadedPolicy, &context(requirements()), &workers, 10).unwrap();
        assert_eq!(selected.identity.worker_id, "a");
    }

    #[test]
    fn warm_shape_is_a_soft_preference_before_load() {
        // FMECA (S=severity, O=occurrence, D=detection; 1..10):
        // F1 stale/missing receipt selects a cold Worker (S3 O4 D2, RPN24):
        // acceptable latency degradation, never an eligibility failure.
        // F2 receipt treated as hard capability (S8 O2 D3, RPN48): prevented by
        // applying it only inside ranking after the compatibility kernel.
        // F3 lower-load cold Worker defeats useful capacity (S4 O5 D2, RPN40):
        // prevented by ordering warm match before in-flight count.
        //
        // Cause-effect graph: C1=request has preferred shape; C2=worker has exact
        // receipt; C3=worker is otherwise eligible; C4=worker has lower load.
        // Effects: E1=warm worker ranks first; E2=least-loaded fallback; E3=no
        // eligible worker is excluded. Derived decision table:
        // | Rule | C1 | C2(any) | C3 | C4(cold) | Effect |
        // | P1   | 1  | 1       | 1  | 1        | E1     |
        // | P2   | 1  | 0       | 1  | 1        | E2,E3  |
        // | P3   | 0  | -       | 1  | 1        | E2,E3  |
        let mut warm = manifest("warm", 3);
        warm.warm_environment_shapes.insert("shape-a".into());
        let cold = manifest("cold", 0);
        let mut preferred = context(requirements());
        preferred.attributes.insert(
            PREFERRED_ENVIRONMENT_SHAPE_ATTRIBUTE.into(),
            "shape-a".into(),
        );
        assert_eq!(
            place(&LeastLoadedPolicy, &preferred, &[cold.clone(), warm], 10)
                .unwrap()
                .identity
                .worker_id,
            "warm",
            "P1"
        );

        assert_eq!(
            place(
                &LeastLoadedPolicy,
                &preferred,
                &[cold.clone(), manifest("other", 2)],
                10
            )
            .unwrap()
            .identity
            .worker_id,
            "cold",
            "P2"
        );
        assert_eq!(
            place(
                &LeastLoadedPolicy,
                &context(requirements()),
                &[cold, manifest("other", 2)],
                10
            )
            .unwrap()
            .identity
            .worker_id,
            "cold",
            "P3"
        );
    }

    struct InjectingPolicy;
    impl PlacementPolicy for InjectingPolicy {
        fn id(&self) -> &str {
            "injecting"
        }

        fn rank(
            &self,
            _context: &PlacementContext,
            _eligible: &[WorkerSnapshot],
        ) -> Result<Vec<RankedWorker>, PlacementError> {
            Ok(vec![RankedWorker {
                identity: WorkerIdentity::new("evil", "boot", 1),
                score: i64::MAX,
                reason: "bypass".to_string(),
            }])
        }
    }

    #[test]
    fn extension_cannot_inject_an_ineligible_worker() {
        let error = place(
            &InjectingPolicy,
            &context(requirements()),
            &[manifest("a", 0)],
            10,
        )
        .unwrap_err();
        assert!(matches!(error, PlacementError::IneligibleResult(_)));
    }

    #[test]
    fn stale_draining_full_or_tampered_workers_are_filtered() {
        let mut stale = manifest("stale", 0);
        stale.expires_at_ms = 10;
        let mut draining = manifest("draining", 0);
        draining.state = WorkerState::Draining;
        let full = manifest("full", 4);
        let mut tampered = manifest("tampered", 0);
        tampered.capability_fingerprint = "sha256:bad".to_string();
        assert!(matches!(
            place(
                &LeastLoadedPolicy,
                &context(requirements()),
                &[stale, draining, full, tampered],
                10,
            ),
            Err(PlacementError::NoEligibleWorker)
        ));
    }

    #[test]
    fn legacy_defaults_and_strict_builder_are_explicit() {
        let legacy: PlacementRequirements = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy.contract_version, 0);
        assert_eq!(legacy.location, ExecutionLocation::RemotePreferred);
        let strict = PlacementRequirements::remote_required();
        assert_eq!(strict.contract_version, CURRENT_CONTRACT_VERSION);
        assert_eq!(strict.location, ExecutionLocation::RemoteRequired);
    }

    #[test]
    fn fingerprint_is_stable_and_sensitive_to_manifest_changes() {
        let mut first = manifest("a", 0).manifest;
        let same = first.clone();
        assert_eq!(first.fingerprint().unwrap(), same.fingerprint().unwrap());
        let old = first.fingerprint().unwrap();
        first.capabilities.insert("gpu".to_string());
        assert_ne!(old, first.fingerprint().unwrap());
    }

    #[test]
    fn recovery_mode_controls_cross_incarnation_assignment() {
        let first = manifest("worker-a", 0);
        let replacement = manifest("worker-b", 0);
        let previous = WorkerAssignment::from(&first);

        let mut never = requirements();
        never.recovery = WorkerRecoveryMode::NeverReplace;
        assert_eq!(
            can_assign(&replacement, &never, Some(&previous), true, 10),
            Err(AssignmentRejection::ReplacementForbidden)
        );
        assert!(can_assign(&first, &never, Some(&previous), false, 10).is_ok());

        let mut continuity = requirements();
        continuity.recovery = WorkerRecoveryMode::RequireSandboxContinuity;
        assert_eq!(
            can_assign(&replacement, &continuity, Some(&previous), false, 10),
            Err(AssignmentRejection::SandboxContinuityUnavailable)
        );
        assert!(can_assign(&replacement, &continuity, Some(&previous), true, 10).is_ok());

        let rebuild = requirements();
        assert!(can_assign(&replacement, &rebuild, Some(&previous), false, 10).is_ok());
    }

    proptest! {
        #[test]
        fn accepted_capabilities_are_always_a_subset(
            offered in prop::collection::btree_set("[a-z]{1,5}", 0..12),
            required in prop::collection::btree_set("[a-z]{1,5}", 0..12),
        ) {
            let manifest = WorkerManifest {
                capabilities: offered.clone(),
                ..WorkerManifest::default()
            };
            let requirements = PlacementRequirements {
                required_capabilities: required.clone(),
                ..PlacementRequirements::default()
            };
            if can_claim(&manifest, &requirements).is_ok() {
                prop_assert!(required.is_subset(&offered));
            }
        }

        #[test]
        fn adding_a_missing_requirement_never_preserves_eligibility(
            offered in prop::collection::btree_set("[a-z]{1,5}", 0..12),
            missing in "[A-Z]{1,5}",
        ) {
            let manifest = WorkerManifest {
                capabilities: offered,
                ..WorkerManifest::default()
            };
            let mut requirements = PlacementRequirements::default();
            requirements.required_capabilities.insert(missing);
            prop_assert!(can_claim(&manifest, &requirements).is_err());
        }
    }
}
