//! Runtime-facing contract: activation data, snapshot execution, and narrow ports.

pub mod activation;
pub mod agent_bindings;
pub mod boundary;
pub mod capability;
pub mod capture;
pub mod control;
pub mod credential;
pub mod data_subject;
pub mod delegation;
pub mod execution;
pub mod inference;
pub mod live_inbox;
pub mod llm;
pub mod metrics;
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
pub mod tool;
pub mod tool_batch;

pub use activation::RunActivation;
pub use boundary::{BoundaryOutcome, evaluate_boundary};
pub use capture::{CaptureDecision, ContentCapture, ContentKind, ContentRedactor, NoopRedactor};
pub use control::LiveRunControl;
pub use credential::{
    AttemptCredentialBinding, AttemptCredentialBindingError, AttemptCredentialRealization,
    CandidateFingerprint, CredentialAccess, CredentialAdmissionError, CredentialEnvelope,
    CredentialExecutionPolicy, CredentialExtensionConsumer, CredentialExtensionDescriptor,
    CredentialExtensionReceipt, CredentialExtensionRequest, CredentialMaterial,
    CredentialMaterialBinding, CredentialMaterialError, CredentialMaterialRequest,
    CredentialMaterialResolver, CredentialMaterialSource, CredentialObservation,
    CredentialObservationSource, CredentialObservationState, CredentialRealizationCapabilities,
    CredentialRealizationKind, CredentialRealizationPlan, CredentialRealizationProfile,
    CredentialRealizationReceipt, CredentialRealizationRecordError, CredentialRealizationRecorder,
    CredentialReceiptError, CredentialRef, CredentialRefreshAccess, CredentialUsage,
    ModelExposurePolicy, OAuthCredentialMaterial, PlaintextBoundary, PlaintextHolder,
    ResolvedCredentialMaterial, SealedCredentialEnvelopeRef, StructuredCredentialMaterial,
    TokenEndpointAuth, TrustDomainRef, WorkerLocalCredentialResolver,
    WorkerLocalReferenceRevalidator, candidate_fingerprint, compile_candidate_credential_bindings,
    verify_credential_realization_receipt,
};
pub use data_subject::{
    CaptureError, CaptureSink, ContentEraser, DataSubjectConsentSource, DataSubjectId,
    DataSubjectResolver, ErasureError, ErasureReceipt, NullResolver, Purpose,
};
pub use delegation::{
    ChildRunCancellation, ChildRunResult, ChildRunResultInbox, DelegationExecutionError,
    DelegationFailureKind, DelegationLimits, DelegationRequest, DelegationResultError,
    DelegationResume, DelegationStep, DelegationToolInput, PendingChildRunResults, ResultRecord,
    RunDelegationService, RunDelegations,
};
pub use execution::{
    A2A_RUNTIME_CAPABILITY, AttemptExecutorRegistry, AttemptExecutorRegistryError, Cancellation,
    ExecutorCapabilities, NATIVE_RUNTIME_CAPABILITY, RunAttemptExecutor, RunExecutor, Wait,
    execution_capability,
};
pub use inference::InferenceExecutorMaterializer;
pub use live_inbox::{LiveInbox, LiveInboxMessage, LiveInboxMessageId};
pub use llm::{ChatRequest, ChatResponse, LlmExecutor};
pub use pause::PauseSignal;
pub use permission::{GateOutcome, ToolGateHook, ToolPermissionPolicy, ToolPermissionVerdict};
pub use plugin::{
    CapabilityBound, Contributions, IdBound, PhaseHook, PhaseHookPoint, Plugin, PluginManifest,
    ResolvedExecutionEnv, RunEndContext, RunEndDecision, RunEndGuard,
};
// The conversation/id types surfaced through this crate's own ports (e.g.
// `RunEndContext.conversation: &[Message]`). Re-exported so an extension that
// consumes those ports names them here, without a direct `agent-contract` edge.
pub use awaken_agent_contract::agent::awaiting::ResumeTicket;
pub use awaken_agent_contract::agent::content::{ContentBlock, ImageSource, extract_text};
pub use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
pub use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
pub use awaken_agent_contract::agent::state::{Command as StateCommand, Key as StateKey};
pub use awaken_agent_contract::agent::state::{MergePolicy, Scope, Store};
pub use awaken_agent_contract::agent::thread::Id as ThreadId;
// Credential material ports expose this opaque value in their signatures.
// Re-export it with the rest of the runtime-facing agent types so boundary
// adapters do not need a second, upward dependency on the agent contract.
pub use awaken_agent_contract::RedactedString;
pub use awaken_agent_contract::thread::commit::coordinator::{
    Coordinator as CommitCoordinator, Error as CommitError,
    OperationCoordinator as CommitOperationCoordinator,
};
pub use awaken_agent_contract::thread::commit::operation::{
    CommitOperation, CommitOperationId, CommitPayloadHash, CommitReceipt,
};
pub use awaken_agent_contract::thread::commit::staged::{
    CommitRecord, RunDisposition, ThreadCommit,
};
pub use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
pub use awaken_agent_contract::thread::read::transcript::{
    TranscriptRange, TranscriptSliceSpec, TranscriptSnapshotRef, TranscriptView,
};
// The cancellation token surfaced through `RunEndContext`/`RuntimeRunContext`;
// re-exported so an extension forwards it without a direct `tokio-util` edge.
pub use resolution::{
    ResolutionManifest, ResolutionManifestError, ResolvedInputRef, ResolvedInputVersion,
    content_fingerprint,
};
pub use resolved::{CatalogFingerprint, InferenceEndpoint, ModelBinding, ResolvedSpec};
pub use resolver::{
    AgentSnapshotResolver, PublishedAgentSnapshotSource, RunResolver, StaticPublishedAgentSnapshots,
};
pub use resume::{ResumeCommand, ResumeError, ResumeResult, validate_resume};
pub use runtime_context::{
    AttemptOwnershipError, AttemptOwnershipVerifier, CaptureContext, RuntimeRunContext,
};
pub use snapshot::{
    AgentConfigRevisionRef, AgentPublicationVersion, AgentSnapshotFingerprint,
    AgentSnapshotMetadata, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
pub use snapshot_builder::ExecutableAgentSnapshotBuilder;
pub use tokio_util::sync::CancellationToken;
pub use tool::{
    RawTool, RawToolRegistry, Tool, ToolExecutionTarget, ToolExecutor, ToolOutput,
    ToolRecoveryCapability, ToolRecoveryMode, ToolRecoveryPolicy,
};
pub use tool_batch::{
    ActiveToolBatch, ToolBatch, ToolBatchId, ToolBatchPhase, ToolCallPhase, ToolWait, ToolWaitKind,
};
