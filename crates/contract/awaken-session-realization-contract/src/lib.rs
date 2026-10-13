//! Session realization commands, projections, and control ports.

mod claimed_control;
#[cfg(feature = "test-support")]
pub mod realization_observation;
mod resource_operation_fence;
mod session_realization;

#[cfg(test)]
pub(crate) use awaken_environment_contract::EnvironmentRevision;
#[cfg(feature = "test-support")]
pub(crate) use awaken_session_configuration_contract::mcp::McpGeneration;
pub(crate) use awaken_session_configuration_contract::mcp::{
    McpGenerationRef, McpRealizationReceipt, SessionMcpAttachment, StageMcpAttachment,
};
pub(crate) use awaken_session_configuration_contract::resource::{
    ResolvedInput, ResolvedSessionResources, ResolvedSkillBinding, SessionRealizationLease,
    SessionResourceManifest, SessionResourceTransition,
};
#[cfg(test)]
pub(crate) use awaken_session_configuration_contract::{
    EnvironmentFingerprint, EnvironmentSnapshot, SessionBaselineInputs, SessionNetworkPolicy,
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
pub(crate) use {
    awaken_session_configuration_contract::{
        SessionBaseline, SessionInit, SessionModelOverride, SessionSandboxLayout,
        SessionSystemPromptSelection, SessionToolConfiguration,
    },
    awaken_session_types::SessionRuntimePlacement,
};

pub use claimed_control::{ClaimedSessionControlError, ClaimedSessionRealizationControl};
pub use resource_operation_fence::ResourceOperationFence;
pub use session_realization::*;

#[cfg(test)]
mod mcp_attachment {
    pub(crate) fn test_generation(
        attachment: &str,
        version: u64,
        url: &str,
    ) -> awaken_session_configuration_contract::mcp::McpGeneration {
        awaken_session_configuration_contract::mcp::McpGeneration::of_bare_target(
            awaken_session_configuration_contract::mcp::McpAttachmentId(attachment.into()),
            version,
            &awaken_agent_contract::McpTarget::parse_http(url).unwrap(),
        )
    }
}
