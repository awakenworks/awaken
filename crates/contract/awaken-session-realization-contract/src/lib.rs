//! Session realization commands, projections, and control ports.

mod claimed_control;
#[cfg(feature = "test-support")]
pub mod realization_observation;
mod resource_operation_fence;
mod session_realization;

#[cfg(test)]
pub(crate) use awaken_environment_contract::EnvironmentRevision;
#[cfg(test)]
pub(crate) use awaken_session_configuration_contract::{
    EnvironmentFingerprint, EnvironmentSnapshot, SessionBaselineInputs, SessionNetworkPolicy,
};
pub(crate) use awaken_session_configuration_contract::{
    SessionBaseline, SessionInit, SessionModelOverride, SessionRuntimePlacement,
    SessionSandboxLayout, SessionSystemPromptSelection, SessionToolConfiguration,
};
#[cfg(feature = "test-support")]
pub(crate) use awaken_session_mcp_contract::McpGeneration;
pub(crate) use awaken_session_mcp_contract::{
    McpGenerationRef, McpRealizationReceipt, SessionMcpAttachment, StageMcpAttachment,
};
pub(crate) use awaken_session_resource_contract::{
    ResolvedInput, ResolvedSessionResources, ResolvedSkillBinding, SessionRealizationLease,
    SessionResourceManifest, SessionResourceTransition,
};
#[cfg(test)]
pub(crate) use awaken_session_state_contract::SessionCleanupCommand;
pub(crate) use awaken_session_state_contract::{
    SessionCleanupDisposalReceipt, SessionCleanupPreparation, SessionEnvironmentOperation,
    SessionEnvironmentState, SessionRepositoryPublicationCommand,
    SessionRepositoryPublicationReceipt, SessionRepositoryPublicationRejection,
    SessionTerminalCleanupAction, SessionTerminalCleanupDisposalEffect,
    SessionTerminalCleanupEffect, SessionTerminalMemoryIntent, SessionTerminalMemoryTarget,
};
pub(crate) use awaken_session_types::RunError;
pub(crate) use awaken_session_types::{SessionId, SessionRevision};

pub use claimed_control::{ClaimedSessionControlError, ClaimedSessionRealizationControl};
pub use resource_operation_fence::ResourceOperationFence;
pub use session_realization::*;

#[cfg(test)]
mod mcp_attachment {
    pub(crate) fn test_generation(
        attachment: &str,
        version: u64,
        url: &str,
    ) -> awaken_session_mcp_contract::McpGeneration {
        awaken_session_mcp_contract::McpGeneration::of_bare_target(
            awaken_session_mcp_contract::McpAttachmentId(attachment.into()),
            version,
            &awaken_session_mcp_contract::McpTarget::parse_http(url).unwrap(),
        )
    }
}
