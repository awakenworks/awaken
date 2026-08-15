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
mod dream;
mod environment;
mod lifecycle;
mod mcp_attachment;
mod mcp_probe;
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
    AgentToolPermissionPolicy, AgentToolsetMember, CustomToolInputSchema, ObjectSchemaKind,
    agent_toolset_members, is_agent_toolset_member, resolved_toolsets, toolset_policies,
};
pub use awaken_agent_contract::stable_fingerprint;
pub use awaken_environment_contract::{EnvironmentPackages, EnvironmentRevision};
pub use baseline::{
    CompiledSessionCreation, ControlSessionCreationInputs, EnvironmentCheckpointExpiryBehavior,
    EnvironmentFingerprint, EnvironmentIdleRetentionMode, EnvironmentIdleRetentionPolicy,
    EnvironmentSnapshot, SandboxProvisioning, SessionBaseline, SessionBaselineFingerprint,
    SessionBaselineInputs, SessionBaselineState, SessionCreationFinalizeError,
    SessionCreationIntent, SessionMcpAuthoringContext, SessionNetworkPolicy,
    SessionRuntimePlacement, resolved_environment_snapshot_is_exact,
};
pub use budget::{
    ManagedBudgetUsageCursor, ManagedListPriceError, ManagedListPriceProvider,
    ManagedListPriceRequest, ManagedListPriceSnapshot, ManagedModelUsageCursor,
    ManagedRuntimeListRates, ManagedTokenListRates, SessionBudgetState,
};

/// Rebuildable desired Environment capacity exported by Coordinator. The
/// executable Environment catalog remains the sole source of desired state;
/// implementations must not persist a parallel warmup queue.
#[async_trait::async_trait]
pub trait EnvironmentWarmupSource: Send + Sync {
    async fn current_environment_warmups(&self) -> Result<Vec<EnvironmentSnapshot>, String>;
}
pub use dream::{
    DREAM_MAX_INSTRUCTIONS_CHARS, DREAM_MAX_SESSIONS, DREAM_SUPPORTED_MODELS, Dream,
    DreamCreateParams, DreamError, DreamInput, DreamListParams, DreamModelConfig, DreamModelInput,
    DreamModelSpeed, DreamOutput, DreamPage, DreamPolicy, DreamPolicyApplication,
    DreamPolicyApplicationError, DreamPolicyConfig, DreamPolicyRecord, DreamProcessFailure,
    DreamProcessRecord, DreamProcessStore, DreamProcessStoreError, DreamStatus, DreamUsage,
};
pub use environment::{
    CheckpointReceipt, QuiescenceReceipt, RestoreReceipt, SandboxCheckpointRef,
    SandboxCheckpointRequest, SandboxGeneration, SessionEnvironmentEffectKind,
    SessionEnvironmentOperation, SessionEnvironmentReceipt, SessionEnvironmentReceiptError,
    SessionEnvironmentState, SessionEnvironmentTransitionError, SourceDisposedReceipt,
    SuspendPhase, checkpoint_source_disposal_authorized,
};
pub use lifecycle::{
    CompositeLifecycleFactDelivery, LifecycleFactDelivery, LifecycleFactNotifier,
    ManagedLifecycleFact, SessionRuntimeInterval, SessionRuntimeIntervalStart,
};
pub use mcp_attachment::{
    McpAttachmentDraft, McpAttachmentError, McpAttachmentId, McpAttachmentOrigin,
    McpAttachmentState, McpDesiredSetFingerprint, McpGeneration, McpGenerationRef,
    McpProjectionEffectKind, McpProjectionReceipt, McpRealizationClaim, McpRealizationReceipt,
    McpRealizationReceiptError, McpReplacementPlan, McpSetRevision, McpTarget, McpTargetError,
    McpTargetIdentity, SessionMcpAttachment, SessionMcpAttachmentSet, StageMcpAttachment,
};
pub use mcp_probe::{McpProbe, McpProbeStatus};
pub use resource::{
    ResolvedInput, ResolvedInputSource, ResolvedRepositoryCredential, ResolvedSessionResources,
    ResolvedSkillBinding, SessionInputAttachment, SessionInputError, SessionInputResolver,
    SessionResourceManifest, repository_transport_credential_usage,
};
pub use resource_activation::{
    ActivationState, ResourceActivationError, SessionResourceActivation, SessionResourceState,
};
pub use run_application::{
    CursorParams, EventForwardingSink, HistoryPage, RunApplication, RunApplicationError, RunResume,
    blocks_text, epoch_millis_to_rfc3339, paginate_history,
};
pub use session::{
    AgentCapabilities, BuiltinTool, CustomTool, DelegatedRun, DelegatedRunSnapshot,
    LiveInboxApplication, LiveInboxApplicationError, LiveInboxEntry, LiveInboxError,
    LiveInboxSnapshot, McpAttachmentRealizer, OutcomeDrive, OutcomeIteration, OutcomeReport,
    Pending, RunError, RunErrorKind, SessionEnvironmentBindingSink, SessionInit, SessionModelUsage,
    SessionRuntime, SessionUsage, StepOutcome, ToolPermissionDecision,
};
pub use session_realization::{
    AcknowledgeSessionRealization, ActivateSessionRealization, BeginSessionRealization,
    FailSessionRealization, FrozenAgentPublicationDecision, FrozenSessionProjection,
    SessionProjectionSynchronizer, SessionRealizationAction, SessionRealizationControl,
    SessionRealizationControlDisposition, SessionRealizationControlFailure,
    SessionRealizationDirective, SessionRealizationDriveError, SessionRealizationProgress,
    SessionRealizationTarget, drive_session_realization, frozen_agent_publication_decision,
    realization_generation_authorizes, realization_lease_authorizes, realization_lease_is_live_at,
};
pub use session_repo::{
    IdempotencyRecord, ManagedSessionRepository, PersistedSession, ScopedPersistedSession,
    SessionDisposition, SessionDispositionTransitionError, SessionExecutionState,
    SessionExecutionStateError, SessionExecutionTransitionError, SessionIdempotencyReceipt,
    SessionMutation, SessionMutationPayload, SessionMutationResult, SessionMutationValidationError,
    SessionRealizationLease, SessionRecoveryQuarantine, SessionRecoveryScan,
    SessionRepositoryConflict, SessionRepositoryError, SessionRepositoryRecoveryAction,
    SessionRevision, SessionTombstone, VisibleMcpServer,
};
pub use skill_execution::{
    SkillBundleSource, SkillBundleSourceError, SkillCatalogApplication, SkillExecutionPin,
    validate_skill_bundle,
};
pub use terminal_cleanup::{
    SessionTerminalCleanupError, SessionTerminalCleanupIntent, SessionTerminalCleanupReceipt,
    SessionTerminalCleanupState,
};
pub use tool_configuration::SessionToolConfiguration;
