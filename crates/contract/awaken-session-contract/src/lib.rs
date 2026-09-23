//! `awaken-session-contract` — the neutral Session APIs, SPIs, and vocabulary.
//!
//! The seam between the Managed Agents wire adapter (`awaken-protocol-managed`) and
//! the service layer that implements it (`awaken-session-executor`): the APIs a host
//! implements (session runtime, work queue, MCP probe, agent-config source, session
//! repository) plus the neutral vocabulary in their signatures. Dependencies point
//! inward — this is a `contract/` leaf, so the host and other implementors depend on
//! it instead of reverse-depending on a protocol adapter.
//!
//! Protocol adapters consume these contracts directly; this crate remains the
//! single definition site for neutral Session and cross-Session process semantics.

#[cfg(feature = "test-support")]
pub mod realization_observation;

mod coordination;
mod environment;
mod event_admission;
mod executable_projection;
pub use event_admission::{
    DeletedSessionEventAdmissions, RetainedEventAdmission, SessionEventAdmission,
    event_admission_closed, event_admission_conflict,
};
mod lifecycle;
mod resource_activation;
mod run_admission;
mod run_application;
mod session;
mod session_realization;
mod session_repo;
mod terminal_cleanup;
mod tool_configuration;
pub mod work_queue {
    pub use awaken_session_work_contract::*;
}

#[cfg(test)]
mod mcp_attachment {
    pub(crate) fn test_generation(
        attachment: &str,
        version: u64,
        url: &str,
    ) -> crate::McpGeneration {
        crate::McpGeneration::of_bare_target(
            crate::McpAttachmentId(attachment.into()),
            version,
            &crate::McpTarget::parse_http(url).unwrap(),
        )
    }

    pub(crate) fn test_generation_at_version(
        generation: &crate::McpGeneration,
        version: u64,
    ) -> crate::McpGeneration {
        generation.at_version(version)
    }
}

/// Resource-plane vocabulary used by Session APIs. Runtime implementors can
/// consume these signatures through this contract instead of adding another
/// dependency edge to the Resources context.
pub mod resource_plane {
    pub use awaken_resource_contract::*;
}

pub use awaken_agent_contract::{
    CanonicalJsonHashError, canonical_json_sha256, stable_fingerprint,
};
pub use awaken_environment_contract::{EnvironmentPackages, EnvironmentRevision};
pub use awaken_session_budget_contract::{
    BudgetReachTransition, ManagedBudgetUsageCursor, ManagedListPriceError,
    ManagedListPriceProvider, ManagedListPriceRequest, ManagedListPriceSnapshot,
    ManagedModelUsageCursor, ManagedRuntimeListRates, ManagedTokenListRates, SessionBudgetState,
    SessionModelUsage, SessionUsage,
};
pub use awaken_session_configuration_contract::{
    AGENT_TOOLSET_TOOL_IDS, AgentPermissionPreset, AgentPermissionPresetTarget, AgentTool,
    AgentToolConfig, AgentToolDefaultConfig, AgentToolPermissionPolicy, AgentToolsetMember,
    AgentWebSearchUserLocation, AgentWebSearchUserLocationKind, CompiledSessionCreation,
    ControlSessionCreationInputs, CredentialRefreshExchange, CustomToolInputSchema,
    EnvironmentCheckpointExpiryBehavior, EnvironmentFingerprint, EnvironmentIdleRetentionMode,
    EnvironmentIdleRetentionPolicy, EnvironmentSnapshot, EnvironmentWarmupSource, McpInspection,
    McpProbe, McpProbeFailure, McpProbeStatus, McpProbeTool, ObjectSchemaKind,
    PROBE_HTTP_BODY_LIMIT, ProbeHttpResponse, RefreshExchangeOutcome, RefreshExchangeStatus,
    SandboxProvisioning, SessionBaseline, SessionBaselineFingerprint, SessionBaselineInputs,
    SessionBaselineState, SessionCreationFinalizeError, SessionCreationIntent,
    SessionMcpAuthoringContext, SessionModelOverride, SessionModelOverrideDecision,
    SessionModelPublication, SessionModelPublicationResolver, SessionModelResolutionError,
    SessionNetworkPolicy, SessionRuntimePlacement, SessionSystemPromptSelection, SkillBundleSource,
    SkillBundleSourceError, SkillCatalogApplication, SkillExecutionPin, agent_toolset_members,
    apply_agent_permission_preset, ensure_typed_mcp_toolset_policies, is_agent_toolset_member,
    is_controlled_modification_member, preserve_runtime_agent_overrides,
    resolved_environment_snapshot_is_exact, resolved_toolsets, session_model_override_decision,
    toolset_policies, validate_agent_tools, validate_mcp_toolset_pairing, validate_skill_bundle,
};
pub use coordination::{
    CoordinatedRunCommand, CoordinatedRunIntent, CoordinatedThreadLink, CoordinatedThreadTarget,
    SessionAgentBoundaryAction, SessionAgentBoundaryCommand, SessionAgentBoundaryRole,
    SessionAgentBoundaryVisibility, SessionAgentCommittedReport, SessionAgentCoordination,
    SessionAgentMessageCommand, SessionAgentMessageReceipt, SessionAgentReportContinuation,
    SessionAgentRosterEntry, SessionAgentRunBoundary, SessionAgentTarget,
    SessionRunActivityAdmission, SessionRunActivityAdmissionMode, SessionThreadTarget,
    SessionThreadToolReply, SessionThreadToolReplyActivity, SessionThreadToolReplyCommand,
    SessionThreadToolReplyDelivery, SessionThreadToolReplyFence, coordinated_thread_failed,
    coordinated_thread_id, session_agent_boundary_action, session_agent_report_messages,
    session_agent_report_text, session_agent_run_boundary, session_run_activity_operation_id,
};
pub use executable_projection::{
    ExecutableAgentProjectionRefresh, ExecutableEnvironmentProjectionRefresh,
    ExecutableProjectionAdmission, ExecutableProjectionRefresh,
    process_local_executable_projection_admission,
};

pub use awaken_session_dream_contract::{
    DREAM_MAX_INSTRUCTIONS_CHARS, DREAM_MAX_SESSIONS, DREAM_SUPPORTED_MODELS, Dream,
    DreamCleanupAdmission, DreamCreateParams, DreamError, DreamInput, DreamListParams,
    DreamModelConfig, DreamModelInput, DreamModelSpeed, DreamOutput, DreamOutputBehavior,
    DreamPage, DreamPolicy, DreamPolicyApplication, DreamPolicyApplicationError, DreamPolicyConfig,
    DreamPolicyRecord, DreamProcessFailure, DreamProcessRecord, DreamProcessStore,
    DreamProcessStoreError, DreamStatus, DreamStatusEvent, DreamUsage,
};
pub use awaken_session_event_contract::{
    MAX_SESSION_INITIAL_EVENTS, OUTCOME_BUSY_CODE, SessionEventBatch,
    SessionEventBatchAdmissionAction, SessionEventBatchAdmissionClass,
    SessionEventBatchAdmissionPlan, SessionEventBatchError, SessionEventBatchOperation,
    SessionEventCommand, SessionEventDispatchSettlementPlan, SessionEventDispatchSettlementReceipt,
    SessionEventEntry, SessionEventInput, SessionEventInterrupt, SessionEventProjectionAnchor,
    SessionEventToolReply, SessionEventToolReplyKind, SessionInitialEventPlan,
    SessionOutcomeRubric, SessionUserRunCommand, SessionUserRunSystemInput,
    classify_session_event_batch_admission, decode_session_event_batch_operation,
    resolve_session_event_batch_admission, session_event_batch_id, session_event_batch_operation,
    session_event_dispatch_settlement_plan, session_event_execution_may_close,
    session_event_outcome_id, session_event_user_run_id, session_inputs_allow_idle,
    session_outcome_convenience_id,
};
pub use awaken_session_mcp_contract::{
    McpAttachmentContentDigest, McpAttachmentDraft, McpAttachmentError, McpAttachmentId,
    McpAttachmentOrigin, McpAttachmentState, McpDesiredSetFingerprint, McpGeneration,
    McpGenerationRef, McpProjectionEffectKind, McpProjectionReceipt, McpRealizationClaim,
    McpRealizationReceipt, McpRealizationReceiptError, McpReplacementPlan, McpSetRevision,
    McpTarget, McpTargetError, McpTargetIdentity, SessionMcpAttachment, SessionMcpAttachmentSet,
    StageMcpAttachment,
};
pub use awaken_session_resource_contract::{
    ResolvedInput, ResolvedInputSource, ResolvedRepositoryCredential, ResolvedSessionResources,
    ResolvedSkillBinding, SessionInputAttachment, SessionInputError, SessionInputResolver,
    SessionMutationPolicy, SessionRealizationLease, SessionResourceCompatibilityFacts,
    SessionResourceManifest, SessionResourceTransition, repository_transport_credential_target,
    repository_transport_credential_usage, repository_transport_material_binding,
};
pub use awaken_session_types::{SessionId, SessionRevision};
pub use environment::{
    CheckpointReceipt, QuiescenceReceipt, RestoreReceipt, SandboxCheckpointRef,
    SandboxCheckpointRequest, SandboxGeneration, SandboxRestoreRequest,
    SessionEnvironmentEffectAuthorization, SessionEnvironmentEffectIntent,
    SessionEnvironmentEffectKind, SessionEnvironmentOperation, SessionEnvironmentPhase,
    SessionEnvironmentReceipt, SessionEnvironmentReceiptError,
    SessionEnvironmentReceiptRecoveryAction, SessionEnvironmentState,
    SessionEnvironmentTransitionError, SourceDisposedReceipt, SourceReleaseDisposal,
    SourceReleasePreparationEffect, SourceReleasePreparedReceipt, SuspendPhase,
    checkpoint_source_disposal_authorized, checkpoint_source_preparation_authorized,
};
pub use lifecycle::{
    CompositeLifecycleFactDelivery, LifecycleFactDelivery, LifecycleFactNotifier,
    ManagedLifecycleFact, SessionRuntimeInterval, SessionRuntimeIntervalObservation,
    SessionRuntimeIntervalStart,
};
pub use resource_activation::{
    ActivationState, ResourceActivationError, SessionResourceActivation, SessionResourceReferences,
    SessionResourceState,
};
pub use run_admission::{
    AdmitSessionRun, AdmittedSessionRun, SessionRunActivation, SessionRunActivityReceiptPlan,
    SessionRunCommandFingerprint, SessionRunDelivery, SessionRunExecutionRequirements,
    SessionRunReplacement, SessionRunReservation, session_run_activity_receipt_plan,
    session_run_id,
};
pub use run_application::{
    CursorParams, EventForwardingSink, HistoryPage, RunApplication, RunApplicationError, RunResume,
    SessionRunBackgroundApplication, SessionRunReplacementApplication, blocks_text,
    epoch_millis_to_rfc3339, paginate_history, project_epoch_millis, rfc3339_to_epoch_millis,
};
pub use session::{
    AgentCapabilities, BudgetRuntime, BuiltinTool, CapabilityRuntime, CommittedOutcomeProjection,
    CompleteSessionRuntime, CoordinationRuntime, CustomTool, DelegatedRun, DelegatedRunSnapshot,
    EnvironmentRuntime, LiveInboxApplication, LiveInboxApplicationError, LiveInboxEntry,
    LiveInboxError, LiveInboxRuntime, LiveInboxSnapshot, McpAttachmentRealizer, NeutralRuntime,
    OutcomeDrive, OutcomeFailure, OutcomeIteration, OutcomeReport, OutcomeRuntime, Pending,
    RealizationRuntime, RunError, RunErrorKind, SessionBudgetResumeDelivery,
    SessionBudgetResumeDisposition, SessionBudgetResumeTicket, SessionEnvironmentBindingSink,
    SessionInit, SessionRuntime, SessionSandboxLayout, SessionThreadLiveSubscription, StepOutcome,
    StreamingRunRuntime, TerminalCleanupRuntime, ThreadReadRuntime, ToolPermissionDecision,
    drain_mcp_generation_receipt, publish_mcp_generation_receipt, session_usage_from_thread_usage,
};
pub use session_realization::{
    AcknowledgeSessionRealization, ActivateSessionRealization, BeginSessionRealization,
    CompleteSessionRealizationControl, FailSessionRealization, FrozenAgentPublicationDecision,
    FrozenResourceTransitionUse, FrozenSessionProjection, ObservedSessionRealizationLease,
    RenewSessionRealization, SessionCheckpointArtifactControl, SessionProjectionInstallMode,
    SessionProjectionSynchronizer, SessionRealizationAction, SessionRealizationAssignment,
    SessionRealizationControl, SessionRealizationControlDisposition,
    SessionRealizationControlFailure, SessionRealizationDirective, SessionRealizationDriveError,
    SessionRealizationExclusionDecision, SessionRealizationLeaseReceipt, SessionRealizationPermit,
    SessionRealizationProgress, SessionRealizationTarget, SessionRepositoryPublicationProjection,
    SessionResourceEffectControl, SessionResourceMaterializationControl,
    SessionResourceMaterializationIntent, SessionResourceMaterializationTarget,
    SessionTerminalCleanupAssignment, SessionTerminalCleanupControl,
    SessionTerminalCleanupPreparationAuthorization, SessionTerminalCleanupQueue,
    SessionTerminalCleanupWork, SessionTerminalDisposalControl, SessionTerminalMemoryControl,
    SessionTerminalPreparationControl, SessionTerminalRepositoryPublicationControl,
    WorkerSessionRealizationControl, frozen_agent_publication_decision,
    frozen_sandbox_layout_publication_decision, legacy_agent_publication_decision,
    project_effective_agent_publication, realization_generation_authorizes,
    realization_lease_authorizes, realization_lease_generation_authorizes,
    realization_lease_is_live_at, session_realization_assignment,
    session_realization_exclusion_admits,
};
pub use session_repo::{
    IdempotencyRecord, LifecycleOutbox, PersistedSession, ScopedPersistedSession,
    SessionArchiveWithRepositoryPublicationError, SessionCreateResult, SessionDisposition,
    SessionDispositionTransitionError, SessionEventDispatchSettlementEffect, SessionExecutionState,
    SessionExecutionStateError, SessionExecutionTransitionError, SessionIdempotencyReceipt,
    SessionMutation, SessionMutationPayload, SessionMutationResult, SessionMutationValidationError,
    SessionRecoveryQuarantine, SessionRepository, SessionRepositoryConflict,
    SessionRepositoryError, SessionRepositoryRecoveryAction, SessionScan, SessionScanCursor,
    SessionScanQuery, SessionStore, SessionTombstone, VisibleMcpServer,
};
pub use terminal_cleanup::{
    SessionCleanupCommand, SessionCleanupDisposalCommand, SessionCleanupDisposalReceipt,
    SessionCleanupError, SessionCleanupOperation, SessionCleanupPreparation,
    SessionCleanupRepositoryPreparation, SessionMemoryReconciliationError,
    SessionRepositoryPublicationCommand, SessionRepositoryPublicationEffect,
    SessionRepositoryPublicationIntent, SessionRepositoryPublicationReceipt,
    SessionRepositoryPublicationRejection, SessionTerminalCleanupAction,
    SessionTerminalCleanupDisposalEffect, SessionTerminalCleanupEffect,
    SessionTerminalMemoryIntent, SessionTerminalMemoryTarget,
    terminal_memory_reconciliation_intent, terminal_memory_reconciliation_intents,
    terminal_memory_reconciliation_intents_from_materializations,
    validate_continuation_memory_materializations, validate_continuation_memory_reconciliation,
};
pub use tool_configuration::SessionToolConfiguration;

/// Hidden re-exports so [`neutral_role!`] expands in any crate without that crate
/// naming the contract's dependencies.
#[doc(hidden)]
pub mod __macro_support {
    pub use async_trait;
    pub use awaken_agent_contract;
}
