//! Immutable Session configuration and its resolution ports.
//!
//! Sandbox provisioning and idle-retention values are named from their owner,
//! `awaken-provisioning-contract`, under their own names:
//!
//! ```compile_fail
//! use awaken_session_configuration_contract::EnvironmentIdleRetentionMode;
//! ```
//!
//! # Boundary
//!
//! Between the Control plane and the Session (immutable configuration and its resolution ports).
//!
//! Implemented by: `awaken-config-service`, `awaken-control`, `awaken-coordinator`,
//! `awaken-environment-application`, `awaken-resource-application`, `awaken-resource-worker-http`,
//! `awaken-scenario-host`, `awaken-session-application`.
//!
//! Called by: `awaken-admin-assistant`, `awaken-config-service`, `awaken-control`,
//! `awaken-coordinator`, `awaken-coordinator-client`, `awaken-dispatch-contract`,
//! `awaken-dispatch-execution`, `awaken-dispatch-http`, `awaken-environment-application`,
//! `awaken-protocol-managed`, `awaken-protocol-managed-testkit`, `awaken-resource-application`,
//! `awaken-resource-worker-http`, `awaken-scenario-host`, `awaken-session-application`,
//! `awaken-session-contract`, `awaken-session-environment`, `awaken-session-execution-contract`,
//! `awaken-session-executor`, `awaken-session-mcp`, `awaken-session-realization`,
//! `awaken-session-realization-driver`, `awaken-session-run-delivery`, `awaken-session-store`,
//! `awaken-store-conformance`, `awaken-worker`.

mod accepted_agent;
pub use accepted_agent::{
    SessionAgentConfiguration, SessionAgentRoster, SessionAgentRosterMember, SessionModelChoice,
    SessionModelSource, VisibleMcpServer,
};
mod agent_config;
mod application_origin;
mod baseline;
mod inspection;
pub mod managed_layout;
pub use inspection::{SessionConfigurationInspection, publication_inspection};
mod dispatched_runtime;
pub mod environment_projection;
mod execution_context;
pub mod mcp;
mod mcp_probe;
mod model_resolution;
mod network_policy;
pub mod resource;
mod runtime_projection;
pub mod sandbox_layout;
pub mod skill_delivery;
mod skill_execution;
mod tool_configuration;

use crate::mcp::McpAttachmentDraft;
#[cfg(test)]
use crate::resource::{ResolvedInput, ResolvedInputSource};
use crate::resource::{ResolvedSessionResources, ResolvedSkillBinding, SessionMutationPolicy};
#[cfg(test)]
use {crate::mcp::McpAttachmentOrigin, awaken_agent_contract::McpTarget};

pub use agent_config::{
    AGENT_TOOLSET_TOOL_IDS, AgentPermissionPreset, AgentPermissionPresetTarget,
    BuiltinToolsetMember, agent_toolset_members, apply_agent_permission_preset,
    ensure_typed_mcp_toolset_policies, hand_recovery_modes, is_agent_toolset_member,
    is_controlled_modification_member, is_hand_member, preserve_runtime_agent_overrides,
    toolset_default_policy, validate_mcp_toolset_pairing,
};
pub use application_origin::{ApplicationOriginBinding, ApplicationOriginBindingError};
pub use baseline::{
    CompiledSessionCreation, ControlSessionCreationInputs, EnvironmentFingerprint,
    EnvironmentSnapshot, SessionBaseline, SessionBaselineFingerprint, SessionBaselineInputs,
    SessionBaselineState, SessionCreationFinalizeError, SessionCreationIntent,
    SessionMcpAuthoringContext, SessionSystemPromptSelection,
    resolved_environment_snapshot_is_exact,
};
pub use dispatched_runtime::{SessionRuntimeDispatchError, SessionRuntimeProjection};
pub use execution_context::{SessionContextError, SessionContextSeed, SessionTrigger};
pub use mcp_probe::{
    CredentialRefreshExchange, McpInspection, McpProbe, McpProbeFailure, McpProbeStatus,
    McpProbeTool, PROBE_HTTP_BODY_LIMIT, ProbeHttpResponse, RefreshExchangeOutcome,
    RefreshExchangeStatus, scrub_sensitive_keys,
};
pub use model_resolution::{
    SessionModelOverride, SessionModelOverrideDecision, SessionModelPublication,
    SessionModelPublicationResolver, SessionModelResolutionError, session_model_override_decision,
};
pub use network_policy::SessionNetworkPolicy;
pub use runtime_projection::{SessionInit, SessionSandboxLayout};
pub use skill_execution::{
    SkillBundleSource, SkillBundleSourceError, SkillCatalogApplication, SkillExecutionPin,
    validate_skill_bundle,
};
pub use tool_configuration::{
    AgentCapabilities, BuiltinTool, CustomTool, SessionToolConfiguration,
};

pub(crate) use awaken_agent_contract::stable_fingerprint;

/// Rebuildable desired Environment capacity exported by Coordinator.
#[async_trait::async_trait]
pub trait EnvironmentWarmupSource: Send + Sync {
    async fn current_environment_warmups(&self) -> Result<Vec<EnvironmentSnapshot>, String>;
}

use awaken_session_types::SessionRuntimePlacement;

mod worker_provisioning;
pub use worker_provisioning::{is_native_session_runtime, validate_sandbox_provisioning_runtime};
