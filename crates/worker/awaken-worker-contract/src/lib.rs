//! Neutral worker identity, compatibility, and placement-policy contract.
//!
//! This crate deliberately separates the non-replaceable eligibility kernel from
//! replaceable ranking policy. A policy may order workers that already satisfy the
//! durable requirements, but it cannot widen isolation, capability, version, or
//! recovery authority. Registry persistence, authentication, capacity reservation,
//! and executor channels live in server/composition crates.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use async_trait::async_trait;
use awaken_provisioning_contract::{
    IsolationClass, ResourceLimits, SandboxCapabilities, capability_requirements_satisfied,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

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
                secret_egress_substitution: false,
                resource_limits: false,
                custom_rootfs: false,
            },
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
    pub required_zone: Option<String>,
    pub architecture: Option<String>,
    #[serde(default = "workdir_isolation")]
    pub isolation: IsolationClass,
    #[serde(default)]
    pub require_tool_transparent: bool,
    #[serde(default)]
    pub require_network_isolation: bool,
    #[serde(default)]
    pub require_resource_limits: bool,
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
}

const fn workdir_isolation() -> IsolationClass {
    IsolationClass::Workdir
}

impl Default for PlacementRequirements {
    fn default() -> Self {
        Self {
            contract_version: 0,
            required_capabilities: BTreeSet::new(),
            required_zone: None,
            architecture: None,
            isolation: IsolationClass::Workdir,
            require_tool_transparent: false,
            require_network_isolation: false,
            require_resource_limits: false,
            sandbox_backend: None,
            dispatch_contract_version: 0,
            runtime_protocol_version: 0,
            checkpoint_format: None,
            location: ExecutionLocation::RemotePreferred,
            recovery: WorkerRecoveryMode::RebuildFromCommittedTruth,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Incompatibility {
    #[error("local-only work cannot be claimed by a remote worker")]
    LocalOnly,
    #[error("worker capacity must be greater than zero")]
    ZeroCapacity,
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
    #[error("worker is not tool-transparent")]
    ToolTransparency,
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
    if !capability_requirements_satisfied(
        manifest.sandbox.isolation,
        requirements.isolation,
        manifest.sandbox.network_isolation,
        requirements.require_network_isolation,
        manifest.sandbox.resource_limits,
        requirements.require_resource_limits,
    ) {
        return Err(Incompatibility::SandboxCapabilities);
    }
    if requirements.require_tool_transparent && !manifest.sandbox.tool_transparent {
        return Err(Incompatibility::ToolTransparency);
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

/// Registry view consumed by placement. Live executor/channel handles are never
/// stored here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerSnapshot {
    pub identity: WorkerIdentity,
    pub state: WorkerState,
    pub manifest: WorkerManifest,
    pub capability_fingerprint: String,
    pub in_flight: u32,
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
    if !replacing {
        return Ok(());
    }
    match requirements.recovery {
        WorkerRecoveryMode::NeverReplace => Err(AssignmentRejection::ReplacementForbidden),
        WorkerRecoveryMode::RequireSandboxContinuity if !sandbox_bound => {
            Err(AssignmentRejection::SandboxContinuityUnavailable)
        }
        WorkerRecoveryMode::RequireSandboxContinuity
        | WorkerRecoveryMode::RebuildFromCommittedTruth => Ok(()),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerHeartbeat {
    pub sequence: u64,
    pub ready: bool,
    pub in_flight: u32,
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
}

/// Worker-directory authority. Implementations must make every mutation atomic;
/// expired/dead records remain tombstones so late messages cannot resurrect them.
#[async_trait]
pub trait WorkerDirectory: Send + Sync {
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

    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError>;

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
        _context: &PlacementContext,
        eligible: &[WorkerSnapshot],
    ) -> Result<Vec<RankedWorker>, PlacementError> {
        let mut workers = eligible.to_vec();
        workers.sort_by(|left, right| {
            left.in_flight
                .cmp(&right.in_flight)
                .then_with(|| left.identity.cmp(&right.identity))
        });
        Ok(workers
            .into_iter()
            .map(|worker| RankedWorker {
                identity: worker.identity,
                score: -(i64::from(worker.in_flight)),
                reason: "least in-flight work".to_string(),
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
                secret_egress_substitution: true,
                resource_limits: true,
                custom_rootfs: true,
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
            expires_at_ms: 1_000,
        }
    }

    fn requirements() -> PlacementRequirements {
        PlacementRequirements {
            required_capabilities: BTreeSet::from(["github".to_string()]),
            required_zone: Some("cn-a".to_string()),
            architecture: Some("x86_64".to_string()),
            isolation: IsolationClass::Container,
            require_tool_transparent: true,
            require_network_isolation: true,
            require_resource_limits: true,
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
    fn full_manifest_satisfies_full_requirements() {
        assert!(can_claim(&manifest("a", 0).manifest, &requirements()).is_ok());
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
