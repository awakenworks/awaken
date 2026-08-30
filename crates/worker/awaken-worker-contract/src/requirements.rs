use std::collections::BTreeSet;

use awaken_credential_contract::CredentialRef as WorkerCredentialRevision;
use awaken_provisioning_contract::{ResourceRequests, SandboxRequirements};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::manifest::{
    CURRENT_CONTRACT_VERSION, REPOSITORY_CREDENTIALS_CAPABILITY, SESSION_RESOURCES_CAPABILITY,
    WorkerManifest,
};
use crate::observation::WorkerAcpCapabilityRequirement;

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

    /// Compile the static requirements for the existing two-stage terminal
    /// cleanup protocol. Cleanup reopens frozen Session/resource/provider truth;
    /// it does not execute a model and therefore must not inherit Run-only model,
    /// credential, ACP, or tool-recovery demands. The current dispatch-contract
    /// floor is retained because cleanup v2 is carried by the same registered
    /// Worker control transport, although it never claims a `RunDispatch`.
    #[must_use]
    pub fn terminal_cleanup(
        has_session_resources: bool,
        has_credentialed_repository: bool,
        checkpoint_format: Option<String>,
    ) -> Self {
        let mut requirements = Self::remote_required();
        requirements.runtime_protocol_version = 2;
        requirements.checkpoint_format = checkpoint_format;
        if has_session_resources {
            requirements
                .required_capabilities
                .insert(SESSION_RESOURCES_CAPABILITY.to_string());
        }
        if has_credentialed_repository {
            requirements
                .required_capabilities
                .insert(REPOSITORY_CREDENTIALS_CAPABILITY.to_string());
        }
        requirements
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

/// Exact compatibility kernel for one Sandbox-owned tool-recovery demand.
///
/// Placement and the exhaustive proof both call this production selector. A
/// non-default recovery mode is therefore a hard admission axis, rather than a
/// ranking hint that an incompatible Worker could ignore.
#[must_use]
pub const fn sandbox_tool_recovery_is_compatible(
    required: awaken_runtime_contract::tool::ToolRecoveryMode,
    installed: awaken_runtime_contract::tool::ToolRecoveryCapability,
) -> bool {
    required.is_supported_by(installed)
}

/// A Worker manifest may advertise only the recovery capability derived from
/// the executor that this process actually installs.
#[must_use]
pub fn manifest_recovery_matches_installed(
    advertised: awaken_runtime_contract::tool::ToolRecoveryCapability,
    installed: awaken_runtime_contract::tool::ToolRecoveryCapability,
) -> bool {
    advertised == installed
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
        .find(|required| {
            !sandbox_tool_recovery_is_compatible(**required, manifest.sandbox_tool_recovery)
        })
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
