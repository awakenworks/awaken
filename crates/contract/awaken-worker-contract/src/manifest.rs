use std::collections::BTreeSet;

use awaken_provisioning_contract::{IsolationClass, ResourceLimits, SandboxCapabilities};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Worker can install a frozen Workspace-scoped Session resource manifest over
/// shared File/Memory/Skill/lifecycle and Resource Registry ports.
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

/// Worker can revalidate and use exact private credential revisions that never
/// cross the control plane. Use may be secret materialization or a local backend
/// (such as a CLI) reading its own login. Eligibility additionally requires a
/// current observation for every pinned revision, so this capability alone
/// grants no access.
pub const WORKER_LOCAL_CREDENTIALS_CAPABILITY: &str = "worker-local-credentials/v1";

pub const CURRENT_CONTRACT_VERSION: u32 = 1;

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
                control_services: Default::default(),
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
    /// Whether this immutable manifest explicitly advertises the two-stage
    /// terminal-cleanup runtime protocol. `VersionRange::ANY` is the serde
    /// default for historical/omitted manifests, so a zero lower bound cannot
    /// grant this newer destructive-effect protocol even though it contains v2.
    #[must_use]
    pub const fn explicitly_supports_terminal_cleanup_v2(&self) -> bool {
        self.runtime_protocol.min > 0 && self.runtime_protocol.contains(2)
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
