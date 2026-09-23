//! Immutable Session configuration and its resolution ports.

mod agent_config;
mod baseline;
mod mcp_probe;
mod model_resolution;
mod network_policy;
mod skill_execution;

use awaken_session_mcp_contract::McpAttachmentDraft;
#[cfg(test)]
use awaken_session_mcp_contract::{McpAttachmentOrigin, McpTarget};
#[cfg(test)]
use awaken_session_resource_contract::{ResolvedInput, ResolvedInputSource};
use awaken_session_resource_contract::{
    ResolvedSessionResources, ResolvedSkillBinding, SessionMutationPolicy,
};

pub use agent_config::{
    AGENT_TOOLSET_TOOL_IDS, AgentPermissionPreset, AgentPermissionPresetTarget, AgentTool,
    AgentToolConfig, AgentToolDefaultConfig, AgentToolPermissionPolicy, AgentToolsetMember,
    AgentWebSearchUserLocation, AgentWebSearchUserLocationKind, CustomToolInputSchema,
    ObjectSchemaKind, agent_toolset_members, apply_agent_permission_preset,
    ensure_typed_mcp_toolset_policies, is_agent_toolset_member, is_controlled_modification_member,
    preserve_runtime_agent_overrides, resolved_toolsets, toolset_policies, validate_agent_tools,
    validate_mcp_toolset_pairing,
};
pub use baseline::{
    CompiledSessionCreation, ControlSessionCreationInputs, EnvironmentCheckpointExpiryBehavior,
    EnvironmentFingerprint, EnvironmentIdleRetentionMode, EnvironmentIdleRetentionPolicy,
    EnvironmentSnapshot, SandboxProvisioning, SessionBaseline, SessionBaselineFingerprint,
    SessionBaselineInputs, SessionBaselineState, SessionCreationFinalizeError,
    SessionCreationIntent, SessionMcpAuthoringContext, SessionRuntimePlacement,
    SessionSystemPromptSelection, resolved_environment_snapshot_is_exact,
};
pub use mcp_probe::{
    CredentialRefreshExchange, McpInspection, McpProbe, McpProbeFailure, McpProbeStatus,
    McpProbeTool, PROBE_HTTP_BODY_LIMIT, ProbeHttpResponse, RefreshExchangeOutcome,
    RefreshExchangeStatus,
};
pub use model_resolution::{
    SessionModelOverride, SessionModelOverrideDecision, SessionModelPublication,
    SessionModelPublicationResolver, SessionModelResolutionError, session_model_override_decision,
};
pub use network_policy::SessionNetworkPolicy;
pub use skill_execution::{
    SkillBundleSource, SkillBundleSourceError, SkillCatalogApplication, SkillExecutionPin,
    validate_skill_bundle,
};

pub(crate) use awaken_agent_contract::stable_fingerprint;

/// Rebuildable desired Environment capacity exported by Coordinator.
#[async_trait::async_trait]
pub trait EnvironmentWarmupSource: Send + Sync {
    async fn current_environment_warmups(&self) -> Result<Vec<EnvironmentSnapshot>, String>;
}
