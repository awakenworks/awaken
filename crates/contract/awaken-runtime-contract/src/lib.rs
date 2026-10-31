//! Runtime-facing contract: activation data, snapshot execution, and narrow ports.

pub mod activation;
pub mod agent_bindings;
pub mod attempt_termination;
pub mod auxiliary_agents;
pub mod boundary;
pub mod cancellation;
pub mod capture;
pub mod compaction;
pub mod control;
pub mod data_subject;
pub mod delegation;
pub mod diagnostic;
pub mod execution;
pub mod live_inbox;
pub mod llm;
pub mod metrics;
pub mod model_inspection;
pub mod model_operation;
mod model_routing;
pub mod pause;
pub mod permission;
pub mod plugin;
pub mod resilience;
pub mod resolution;
pub mod resolved;
pub mod resolver;
pub mod resume;
pub mod runtime_context;
pub mod snapshot;
mod snapshot_builder;
pub mod terminal;

pub use activation::RunActivation;
pub use boundary::{BoundaryOutcome, evaluate_boundary};
pub use capture::{CaptureDecision, ContentCapture, ContentKind, ContentRedactor, NoopRedactor};
pub use control::LiveRunControl;

pub use data_subject::{CaptureError, CaptureSink, DataSubjectId, Purpose};
pub use delegation::{
    ChildRunResult, ChildRunResultInbox, DelegationExecutionError, DelegationFailureKind,
    DelegationRequest, DelegationResultError, DelegationResume, DelegationStep,
    DelegationToolInput, PendingChildRunResults, ResultRecord, RunDelegationService,
    RunDelegations,
};
pub use execution::{
    A2A_RUNTIME_CAPABILITY, AttemptExecutorRegistry, AttemptExecutorRegistryError,
    AttemptRecoveryMode, Cancellation, ExecutorCapabilities, NATIVE_RUNTIME_CAPABILITY,
    RunAttemptExecutor, RunExecutor, Wait, execution_capability,
};
pub use live_inbox::{LiveInbox, LiveInboxMessage, LiveInboxMessageId};
pub use llm::{ChatRequest, ChatResponse, LlmExecutor, ModelContentMaterializer};
pub use pause::PauseSignal;
pub use permission::{GateOutcome, ToolGateHook, ToolPermissionPolicy, ToolPermissionVerdict};
pub use plugin::{
    CapabilityBound, Contributions, IdBound, PhaseHook, PhaseHookPoint, Plugin, PluginManifest,
    ResolvedExecutionEnv, RunEndContext, RunEndDecision, RunEndGuard,
};

pub use resolution::{
    ResolutionManifest, ResolutionManifestError, ResolvedInputRef, ResolvedInputVersion,
};
pub use resolved::{
    CatalogFingerprint, InferenceEndpoint, ModelBinding, ProviderExecutionProfile, ResolvedSpec,
    UnspecifiedReasoning,
};
pub use resolver::{
    AgentSnapshotResolver, PublishedAgentSnapshotSource, RunResolver,
    StaticPublishedAgentSnapshots, freeze_delegation_publications, resolve_delegate_snapshot,
};
pub use resume::{ResumeCommand, ResumeError, ResumeResult, validate_resume};
pub use runtime_context::{
    AttemptOwnershipError, AttemptOwnershipVerifier, CaptureContext, CredentialRealizationHandle,
    ModelInputProjectionTrace, NormalizedModelInput, RuntimeRunContext,
    is_legacy_derived_session_context,
};
pub use snapshot::{
    AgentConfigRevisionRef, AgentPublicationVersion, AgentSnapshotFingerprint,
    AgentSnapshotMetadata, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
pub use snapshot_builder::{
    DEFAULT_MAX_STEPS, ExecutableAgentSnapshotBuilder, deserialize_max_steps,
};
// The cancellation token surfaced through `RunEndContext`/`RuntimeRunContext`;
// re-exported so an extension forwards it without a direct `tokio-util` edge.
pub use tokio_util::sync::CancellationToken;
