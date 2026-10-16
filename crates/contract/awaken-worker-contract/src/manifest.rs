// @formal entrypoint worker.accepted_protocol_version_is_inside_advertised_range
// @formal entrypoint worker.sandbox_tool_recovery_is_a_hard_claim_axis
use std::collections::BTreeSet;

use awaken_provisioning_contract::{ResourceLimits, SandboxCapabilities};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Worker can install a frozen Workspace-scoped Session resource manifest over
/// shared File/Memory/Skill/lifecycle and Resource Registry ports.
pub const SESSION_RESOURCES_CAPABILITY: &str = "session-resources/v1";
/// Terminal preparation decodes and enforces Control's source-I/O admission.
pub const SESSION_TERMINAL_SOURCE_ADMISSION_CAPABILITY: &str =
    "session-terminal-source-admission/v1";

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

/// Worker can revalidate and use exact private credential revisions that never
/// cross the control plane. Use may be secret materialization or a local backend
/// (such as a CLI) reading its own login. Eligibility additionally requires a
/// current observation for every pinned revision, so this capability alone
/// grants no access.
pub const WORKER_LOCAL_CREDENTIALS_CAPABILITY: &str = "worker-local-credentials/v1";

/// Exact Worker backend identity for an operator-authorized host Workdir.
/// Unlike isolation capability, this is not monotonic: Namespace/Container may
/// be stronger sandboxes but cannot substitute for local machine identity.
pub const TRUSTED_HOST_SANDBOX_BACKEND: &str = "trusted-host";

pub const CURRENT_CONTRACT_VERSION: u32 = 1;

/// Current registered Worker execution protocol. V4 retires one complete claim
/// execution after its final ACK and uses typed renewal outcomes. V3 introduced
/// request-relative Session leases, but still ACKed individual scheduled stages.
/// A v3 peer cannot execute safely against whole-claim retirement semantics.
/// Manifest/dispatch envelope versions remain independent and unchanged.
pub const CURRENT_RUNTIME_PROTOCOL_VERSION: u32 = 5;

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

/// A manifest whose `max_concurrent` is zero can never be registered: the
/// scheduler would place no work on it and the execution pool could install no
/// slot. Reading the registered capacity is therefore fallible, and the failure
/// is typed instead of hidden behind an `expect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("registered Worker capacity must be greater than zero")]
pub struct ZeroRegisteredCapacity;

/// The execution capacity one Worker incarnation registered with the registry.
///
/// Placement decisions are made against [`WorkerCapacity::max_concurrent`], so
/// the process that registered it must install exactly that many execution
/// slots. Registered capacity and installed capacity previously diverged in
/// silence because the number travelled as a bare integer that an execution
/// pool was free to ignore in favour of a local CPU guess.
///
/// This newtype is the only currency the pool installer accepts. It has no
/// `Default` and no `Clone`, its sole constructor reads a [`WorkerManifest`],
/// and it is consumed by move when the pool is sized — so it cannot be read
/// twice, cannot be conjured from an unrelated number, and cannot be derived
/// and then quietly dropped: dropping it is an `unused_must_use`, which the
/// workspace's `-D warnings` turns into a build failure.
#[derive(Debug)]
#[must_use = "registered capacity must be installed in the execution pool: dropping it \
              restores a local default and lets registered and actual capacity diverge"]
pub struct RegisteredCapacity(std::num::NonZeroUsize);

impl RegisteredCapacity {
    /// Read the capacity off the manifest this Worker registers. There is no
    /// other constructor: a number that no manifest published cannot become a
    /// `RegisteredCapacity`.
    pub fn of(manifest: &WorkerManifest) -> Result<Self, ZeroRegisteredCapacity> {
        std::num::NonZeroUsize::new(manifest.capacity.max_concurrent as usize)
            .map(Self)
            .ok_or(ZeroRegisteredCapacity)
    }

    /// Consume the registration, yielding the exact number of execution slots to
    /// install. Taking `self` by value is what makes an unused registration a
    /// compiler diagnostic rather than a silent divergence.
    #[must_use]
    pub fn into_slots(self) -> std::num::NonZeroUsize {
        self.0
    }

    /// The registered slot count, for diagnostics and assertions that must not
    /// consume the registration. This never installs anything.
    #[must_use]
    pub const fn slots(&self) -> std::num::NonZeroUsize {
        self.0
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
    /// Additional host-identity backend only. Its guarantees must never be
    /// borrowed from the primary Namespace/Container backend. Omitted legacy
    /// manifests grant only the unenforced Workdir baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_host_sandbox: Option<SandboxCapabilities>,
    /// Trusted recovery capability of the SessionEnvironment-owned Sandbox
    /// executor. Production derives this from the same typed deployment value
    /// that constructs the executor; it is never an Agent-authored claim.
    #[serde(default)]
    pub sandbox_tool_recovery: awaken_tool_contract::tool::ToolRecoveryCapability,
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
            trusted_host_sandbox: None,
            sandbox: SandboxCapabilities::default(),
            sandbox_tool_recovery:
                awaken_tool_contract::tool::ToolRecoveryCapability::NonRecoverable,
            sandbox_backends: BTreeSet::new(),
            dispatch_contract: VersionRange::ANY,
            runtime_protocol: VersionRange::ANY,
            checkpoint_formats: BTreeSet::new(),
            capacity: WorkerCapacity::default(),
        }
    }
}

impl WorkerManifest {
    /// Whether this incarnation explicitly supports the current execution
    /// protocol. Historical/omitted `ANY` ranges carry no positive evidence.
    /// Placement, transport and terminal recovery share this single floor;
    /// neither an old queued requirement nor a broad default can lower it.
    #[must_use]
    pub const fn supports_current_runtime_protocol(&self) -> bool {
        self.runtime_protocol.min > 0
            && self
                .runtime_protocol
                .contains(CURRENT_RUNTIME_PROTOCOL_VERSION)
    }

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
