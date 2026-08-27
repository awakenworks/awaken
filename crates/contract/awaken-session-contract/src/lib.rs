//! `awaken-session-contract` — the neutral Session APIs, SPIs, and vocabulary.
//!
//! The seam between the Managed Agents wire adapter (`awaken-protocol-managed`) and
//! the service layer that implements it (`awaken-runtime-host`): the APIs a host
//! implements (session runtime, work queue, MCP probe, agent-config source, session
//! repository) plus the neutral vocabulary in their signatures. Dependencies point
//! inward — this is a `contract/` leaf, so the host and other implementors depend on
//! it instead of reverse-depending on a protocol adapter.
//!
//! Protocol adapters consume these contracts directly; this crate remains the
//! single definition site for neutral Session and cross-Session process semantics.

mod agent_config;
mod baseline;
mod budget;
mod coordination;
mod dream;
mod environment;
mod event_batches;
mod lifecycle;
mod mcp_attachment;
mod mcp_probe;
mod model_resolution;
mod resource;
mod resource_activation;
mod run_application;
mod session;
mod session_realization;
mod session_repo;
mod skill_execution;
mod terminal_cleanup;
mod tool_configuration;
pub mod work_queue;

/// Resource-plane vocabulary used by Session APIs. Runtime implementors can
/// consume these signatures through this contract instead of adding another
/// dependency edge to the Resources context.
pub mod resource_plane {
    pub use awaken_resource_contract::*;
}

pub use agent_config::{
    AGENT_TOOLSET_TOOL_IDS, AgentTool, AgentToolConfig, AgentToolDefaultConfig,
    AgentToolPermissionPolicy, AgentToolsetMember, AgentWebSearchUserLocation,
    AgentWebSearchUserLocationKind, CustomToolInputSchema, ObjectSchemaKind, agent_toolset_members,
    is_agent_toolset_member, resolved_toolsets, toolset_policies, validate_agent_tools,
};
pub use awaken_agent_contract::stable_fingerprint;
pub use awaken_environment_contract::{EnvironmentPackages, EnvironmentRevision};
pub use baseline::{
    CompiledSessionCreation, ControlSessionCreationInputs, EnvironmentCheckpointExpiryBehavior,
    EnvironmentFingerprint, EnvironmentIdleRetentionMode, EnvironmentIdleRetentionPolicy,
    EnvironmentSnapshot, SandboxProvisioning, SessionBaseline, SessionBaselineFingerprint,
    SessionBaselineInputs, SessionBaselineState, SessionCreationFinalizeError,
    SessionCreationIntent, SessionMcpAuthoringContext, SessionMutationPolicy, SessionNetworkPolicy,
    SessionRuntimePlacement, SessionSystemPromptSelection, resolved_environment_snapshot_is_exact,
};
pub use budget::{
    BudgetReachTransition, ManagedBudgetUsageCursor, ManagedListPriceError,
    ManagedListPriceProvider, ManagedListPriceRequest, ManagedListPriceSnapshot,
    ManagedModelUsageCursor, ManagedRuntimeListRates, ManagedTokenListRates, SessionBudgetState,
};
pub use coordination::{
    CoordinatedRunCommand, CoordinatedRunIntent, CoordinatedThreadLink, CoordinatedThreadTarget,
    SessionAgentBoundaryCommand, SessionAgentCoordination, SessionAgentMessageCommand,
    SessionAgentMessageReceipt, SessionAgentReportContinuation, SessionAgentRosterEntry,
    SessionAgentTarget, SessionRunActivityAdmission, SessionRunActivityAdmissionMode,
    SessionThreadTarget, SessionThreadToolReply, SessionThreadToolReplyCommand,
    SessionThreadToolReplyDelivery, SessionThreadToolReplyFence, coordinated_thread_failed,
    coordinated_thread_id, session_agent_report_messages, session_agent_report_text,
    session_run_activity_operation_id,
};

/// Rebuildable desired Environment capacity exported by Coordinator. The
/// executable Environment catalog remains the sole source of desired state;
/// implementations must not persist a parallel warmup queue.
#[async_trait::async_trait]
pub trait EnvironmentWarmupSource: Send + Sync {
    async fn current_environment_warmups(&self) -> Result<Vec<EnvironmentSnapshot>, String>;
}

/// Process-composed prerequisite that advances rebuildable executable
/// projections before an application boundary consumes current or exact
/// executable facts. Implementations own no cursor, cache, or durable state;
/// those remain with the authoritative projection adapters.
#[async_trait::async_trait]
pub trait ExecutableProjectionRefresh: Send + Sync {
    async fn refresh(&self) -> Result<(), String>;
}
pub use dream::{
    DREAM_MAX_INSTRUCTIONS_CHARS, DREAM_MAX_SESSIONS, DREAM_SUPPORTED_MODELS, Dream,
    DreamCreateParams, DreamError, DreamInput, DreamListParams, DreamModelConfig, DreamModelInput,
    DreamModelSpeed, DreamOutput, DreamOutputBehavior, DreamPage, DreamPolicy,
    DreamPolicyApplication, DreamPolicyApplicationError, DreamPolicyConfig, DreamPolicyRecord,
    DreamProcessFailure, DreamProcessRecord, DreamProcessStore, DreamProcessStoreError,
    DreamStatus, DreamStatusEvent, DreamUsage,
};
pub use environment::{
    CheckpointReceipt, QuiescenceReceipt, RestoreReceipt, SandboxCheckpointRef,
    SandboxCheckpointRequest, SandboxGeneration, SessionEnvironmentEffectKind,
    SessionEnvironmentOperation, SessionEnvironmentReceipt, SessionEnvironmentReceiptError,
    SessionEnvironmentState, SessionEnvironmentTransitionError, SourceDisposedReceipt,
    SuspendPhase, checkpoint_source_disposal_authorized,
};
pub use event_batches::{
    MAX_SESSION_INITIAL_EVENTS, OUTCOME_BUSY_CODE, SessionEventBatch, SessionEventBatchError,
    SessionEventBatchOperation, SessionEventCommand, SessionEventEntry, SessionEventInput,
    SessionEventInterrupt, SessionEventProjectionAnchor, SessionEventToolReply,
    SessionEventToolReplyKind, SessionInitialEventPlan, SessionOutcomeRubric,
    SessionUserRunActivation, SessionUserRunAdmission, SessionUserRunCommand,
    SessionUserRunDelivery, SessionUserRunReservation, SessionUserRunSystemInput,
    decode_session_event_batch_operation, session_event_batch_id, session_event_batch_operation,
    session_event_outcome_id, session_event_user_run_id, session_outcome_convenience_id,
};
pub use lifecycle::{
    CompositeLifecycleFactDelivery, LifecycleFactDelivery, LifecycleFactNotifier,
    ManagedLifecycleFact, SessionRuntimeInterval, SessionRuntimeIntervalObservation,
    SessionRuntimeIntervalStart,
};
pub use mcp_attachment::{
    McpAttachmentDraft, McpAttachmentError, McpAttachmentId, McpAttachmentOrigin,
    McpAttachmentState, McpDesiredSetFingerprint, McpGeneration, McpGenerationRef,
    McpProjectionEffectKind, McpProjectionReceipt, McpRealizationClaim, McpRealizationReceipt,
    McpRealizationReceiptError, McpReplacementPlan, McpSetRevision, McpTarget, McpTargetError,
    McpTargetIdentity, SessionMcpAttachment, SessionMcpAttachmentSet, StageMcpAttachment,
};
pub use mcp_probe::{McpProbe, McpProbeStatus};
pub use model_resolution::{
    SessionModelOverride, SessionModelOverrideDecision, SessionModelPublication,
    SessionModelPublicationResolver, SessionModelResolutionError, session_model_override_decision,
};
pub use resource::{
    ResolvedInput, ResolvedInputSource, ResolvedRepositoryCredential, ResolvedSessionResources,
    ResolvedSkillBinding, SessionInputAttachment, SessionInputError, SessionInputResolver,
    SessionResourceManifest, repository_transport_credential_target,
    repository_transport_credential_usage,
};
pub use resource_activation::{
    ActivationState, ResourceActivationError, SessionResourceActivation, SessionResourceReferences,
    SessionResourceState,
};
pub use run_application::{
    CursorParams, EventForwardingSink, HistoryPage, RunApplication, RunApplicationError, RunResume,
    blocks_text, epoch_millis_to_rfc3339, paginate_history,
};
pub use session::{
    AgentCapabilities, BuiltinTool, CommittedOutcomeProjection, CustomTool, DelegatedRun,
    DelegatedRunSnapshot, LiveInboxApplication, LiveInboxApplicationError, LiveInboxEntry,
    LiveInboxError, LiveInboxSnapshot, McpAttachmentRealizer, OutcomeDrive, OutcomeFailure,
    OutcomeIteration, OutcomeReport, Pending, RunError, RunErrorKind, SessionBudgetResumeDelivery,
    SessionBudgetResumeDisposition, SessionBudgetResumeTicket, SessionEnvironmentBindingSink,
    SessionInit, SessionModelUsage, SessionRuntime, SessionThreadLiveSubscription, SessionUsage,
    StepOutcome, ToolPermissionDecision,
};
pub use session_realization::{
    AcknowledgeSessionRealization, ActivateSessionRealization, BeginSessionRealization,
    FailSessionRealization, FrozenAgentPublicationDecision, FrozenSessionProjection,
    SessionProjectionSynchronizer, SessionRealizationAction, SessionRealizationControl,
    SessionRealizationControlDisposition, SessionRealizationControlFailure,
    SessionRealizationDirective, SessionRealizationDriveError, SessionRealizationProgress,
    SessionRealizationTarget, SessionTerminalCleanupAssignment, drive_session_realization,
    frozen_agent_publication_decision, realization_generation_authorizes,
    realization_lease_authorizes, realization_lease_is_live_at,
};
pub use session_repo::{
    IdempotencyRecord, ManagedSessionRepository, PersistedSession, ScopedPersistedSession,
    SessionCreateResult, SessionDisposition, SessionDispositionTransitionError,
    SessionExecutionState, SessionExecutionStateError, SessionExecutionTransitionError,
    SessionIdempotencyReceipt, SessionMutation, SessionMutationPayload, SessionMutationResult,
    SessionMutationValidationError, SessionRealizationLease, SessionRecoveryQuarantine,
    SessionRecoveryScan, SessionRepositoryConflict, SessionRepositoryError,
    SessionRepositoryRecoveryAction, SessionRevision, SessionTombstone, VisibleMcpServer,
};
pub use skill_execution::{
    SkillBundleSource, SkillBundleSourceError, SkillCatalogApplication, SkillExecutionPin,
    validate_skill_bundle,
};
pub use terminal_cleanup::{
    SessionCleanupCommand, SessionCleanupCompletion, SessionCleanupError, SessionCleanupOperation,
    VerifiedSessionCleanupReceipt,
};
pub use tool_configuration::SessionToolConfiguration;
