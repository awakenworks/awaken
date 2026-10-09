//! Runtime-facing Session roles, coordination, and Run admission.

mod coordination;
mod durable_run_operations;
mod run_admission;
mod session;

pub(crate) use awaken_agent_contract::stable_fingerprint;
#[cfg(test)]
pub(crate) use awaken_session_application_contract::session_usage_from_thread_usage;
pub(crate) use awaken_session_application_contract::{
    DelegatedRun, DelegatedRunSnapshot, Pending, SessionBudgetResumeDelivery,
    SessionBudgetResumeDisposition, SessionBudgetResumeTicket, StepOutcome,
};
pub(crate) use awaken_session_configuration_contract::{
    AgentCapabilities, SessionSandboxLayout, SessionToolConfiguration,
};
#[cfg(test)]
pub(crate) use awaken_session_event_contract::SessionUserRunSystemInput;
#[cfg(test)]
pub(crate) use awaken_session_mcp_contract::McpTarget;
pub(crate) use awaken_session_mcp_contract::{
    McpGenerationRef, McpProjectionEffectKind, McpProjectionReceipt, McpRealizationReceipt,
    StageMcpAttachment,
};
pub(crate) use awaken_session_realization_contract::{
    FrozenSessionProjection, ObservedSessionRealizationLease, SessionProjectionInstallMode,
    SessionRealizationControl, SessionTerminalCleanupAssignment,
    SessionTerminalCleanupPreparationAuthorization,
};
pub(crate) use awaken_session_resource_contract::{
    ResolvedSkillBinding, SessionRealizationLease, SessionResourceTransition,
};
pub(crate) use awaken_session_state_contract::{
    CheckpointReceipt, QuiescenceReceipt, RestoreReceipt, SandboxCheckpointRef,
    SandboxCheckpointRequest, SandboxGeneration, SandboxRestoreRequest,
    SessionCleanupDisposalReceipt, SessionCleanupPreparation,
    SessionEnvironmentEffectAuthorization, SessionEnvironmentEffectIntent,
    SessionEnvironmentOperation, SessionEnvironmentReceipt, SessionEnvironmentState,
    SessionRepositoryPublicationCommand, SessionRepositoryPublicationEffect,
    SessionTerminalCleanupDisposalEffect, SessionTerminalCleanupEffect, SourceDisposedReceipt,
    SourceReleaseDisposal, SourceReleasePreparationEffect, SourceReleasePreparedReceipt,
};
pub(crate) use awaken_session_types::RunError;
#[cfg(test)]
pub(crate) use awaken_session_types::RunErrorKind;
pub(crate) use awaken_session_types::SessionId;

pub use coordination::{
    CoordinatedRunCommand, CoordinatedRunIntent, CoordinatedThreadKind, CoordinatedThreadLink,
    CoordinatedThreadTarget, SessionAgentBoundaryAction, SessionAgentBoundaryCommand,
    SessionAgentBoundaryRole, SessionAgentBoundaryVisibility, SessionAgentCommittedReport,
    SessionAgentCoordination, SessionAgentMessageCommand, SessionAgentMessageReceipt,
    SessionAgentReportContinuation, SessionAgentRosterEntry, SessionAgentRunBoundary,
    SessionAgentTarget, SessionRunActivityAdmission, SessionRunActivityAdmissionMode,
    SessionThreadTarget, SessionThreadToolReply, SessionThreadToolReplyActivity,
    SessionThreadToolReplyCommand, SessionThreadToolReplyDelivery, SessionThreadToolReplyFence,
    coordinated_thread_closed, coordinated_thread_id, session_agent_boundary_action,
    session_agent_report_messages, session_agent_report_text, session_agent_run_boundary,
    session_run_activity_operation_id,
};
pub use durable_run_operations::{
    ApplicationError, ApplicationErrorKind, DurableDispatchStatus, DurableRunOperations,
    DurableSupersedeResult,
};
pub use run_admission::{
    AdmitSessionRun, AdmittedSessionRun, SessionRunActivation, SessionRunActivityReceiptPlan,
    SessionRunCommandFingerprint, SessionRunDelivery, SessionRunExecutionRequirements,
    SessionRunReplacement, SessionRunReservation, session_run_activity_receipt_plan,
    session_run_id,
};
pub use session::{
    BudgetRuntime, CapabilityRuntime, CommittedEvidenceError, CommittedOutcomeProjection,
    CommittedSessionFacts, CompleteSessionRuntime, CoordinationRuntime, EnvironmentRuntime,
    LiveInboxApplication, LiveInboxApplicationError, LiveInboxEntry, LiveInboxError,
    LiveInboxRuntime, LiveInboxSnapshot, McpAttachmentRealizer, NeutralRuntime, OutcomeDrive,
    OutcomeFailure, OutcomeIteration, OutcomeReport, OutcomeRuntime, ProjectionThreadSnapshots,
    RealizationRuntime, SessionEnvironmentBindingSink, SessionProjectionReadTimings,
    SessionProjectionSource, SessionRuntime, SessionThreadLiveSubscription, StreamingRunRuntime,
    TerminalCleanupRuntime, ThreadReadRuntime, ToolPermissionDecision, committed_lifecycle_prefix,
    drain_mcp_generation_receipt, outcome_terminal_commit_cursor, publish_mcp_generation_receipt,
};

#[doc(hidden)]
pub mod __macro_support {
    pub use async_trait;
    pub use awaken_agent_contract;
    pub use awaken_session_application_contract::{
        DelegatedRun, DelegatedRunSnapshot, Pending, SessionBudgetResumeDelivery,
        SessionBudgetResumeDisposition, SessionBudgetResumeTicket, StepOutcome,
    };
    pub use awaken_session_budget_contract::SessionUsage;
    pub use awaken_session_configuration_contract::{
        AgentCapabilities, SessionSandboxLayout, SessionToolConfiguration,
    };
    pub use awaken_session_event_contract::session_outcome_convenience_id;
    pub use awaken_session_mcp_contract::McpGenerationRef;
    pub use awaken_session_realization_contract::{
        FrozenSessionProjection, ObservedSessionRealizationLease, SessionProjectionInstallMode,
        SessionRealizationControl, SessionTerminalCleanupAssignment,
        SessionTerminalCleanupPreparationAuthorization,
    };
    pub use awaken_session_resource_contract::{
        ResolvedSkillBinding, SessionRealizationLease, SessionResourceTransition,
    };
    pub use awaken_session_state_contract::{
        CheckpointReceipt, QuiescenceReceipt, RestoreReceipt, SandboxCheckpointRef,
        SandboxCheckpointRequest, SandboxGeneration, SandboxRestoreRequest,
        SessionCleanupDisposalReceipt, SessionCleanupPreparation, SessionEnvironmentOperation,
        SessionRepositoryPublicationCommand, SessionRepositoryPublicationEffect,
        SessionTerminalCleanupDisposalEffect, SessionTerminalCleanupEffect, SourceDisposedReceipt,
        SourceReleaseDisposal, SourceReleasePreparationEffect, SourceReleasePreparedReceipt,
    };
    pub use awaken_session_types::RunError;
    pub use awaken_session_types::SessionId;
}

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
