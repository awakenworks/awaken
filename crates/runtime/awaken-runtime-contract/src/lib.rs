//! Runtime-facing contract: activation data, snapshot execution, and narrow ports.

pub mod activation;
pub mod agent_resolver;
pub mod boundary;
pub mod capability;
pub mod capture;
pub mod catalog;
pub mod control;
pub mod data_subject;
pub mod execution;
pub mod live_inbox;
pub mod llm;
pub mod metrics;
pub mod pause;
pub mod permission;
pub mod plugin;
pub mod resilience;
pub mod resolved;
pub mod resolver;
pub mod resume;
pub mod runnable;
pub mod runtime_context;
pub mod snapshot;
pub mod subagent_runner;
pub mod tool;

pub use activation::RunActivation;
pub use agent_resolver::{AgentError, AgentRequest, AgentResolver, AgentStep};
pub use boundary::{BoundaryOutcome, evaluate_boundary};
pub use capture::{CaptureDecision, ContentCapture, ContentKind, ContentRedactor, NoopRedactor};
pub use catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
pub use control::LiveRunControl;
pub use data_subject::{
    CaptureSink, ContentEraser, DataSubjectId, DataSubjectResolver, ErasureError, ErasureReceipt,
    NullResolver, Purpose,
};
pub use execution::{Cancellation, ExecutorCapabilities, RunExecutor, Wait};
pub use live_inbox::{LiveInbox, LiveInboxMessage, LiveInboxMessageId};
pub use llm::{ChatRequest, ChatResponse, LlmExecutor};
pub use pause::PauseSignal;
pub use permission::{GateOutcome, PermissionDecision, PermissionPolicy, ToolGateHook};
pub use plugin::{
    CapabilityBound, Contributions, IdBound, PhaseHook, PhaseHookPoint, Plugin, PluginManifest,
    ResolvedExecutionEnv, RunEndContext, RunEndDecision, RunEndGuard,
};
// The conversation/id types surfaced through this crate's own ports (e.g.
// `RunEndContext.conversation: &[Message]`). Re-exported so an extension that
// consumes those ports names them here, without a direct `agent-contract` edge.
pub use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
pub use awaken_agent_contract::agent::run::Id as RunId;
pub use awaken_agent_contract::agent::state::Store;
pub use subagent_runner::{SubagentError, SubagentReply, SubagentRequest, SubagentRunner};
// The cancellation token surfaced through `RunEndContext`/`RuntimeRunContext`;
// re-exported so an extension forwards it without a direct `tokio-util` edge.
pub use resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
pub use resolver::{AgentSnapshotResolver, RunResolver};
pub use resume::{ResumeCommand, ResumeError, ResumeResult, validate_resume};
pub use runnable::{RunnableConfig, RunnableConfigBuilder};
pub use runtime_context::{CaptureContext, RuntimeRunContext};
pub use snapshot::{ExecutableAgentSnapshot, ExecutableAgentSnapshotId};
pub use tokio_util::sync::CancellationToken;
pub use tool::{RawTool, Tool, ToolExecutor, ToolOutput};
