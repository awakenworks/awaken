//! Runtime-facing contract: activation data, snapshot execution, and narrow ports.

pub mod activation;
pub mod capability;
pub mod catalog;
pub mod control;
pub mod execution;
pub mod llm;
pub mod permission;
pub mod plugin;
pub mod plugin_config;
pub mod resolved;
pub mod resolver;
pub mod resume;
pub mod runnable;
pub mod runtime_context;
pub mod snapshot;
pub mod tool;

pub use activation::RunActivation;
pub use catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
pub use control::LiveRunControl;
pub use execution::RunExecutor;
pub use llm::{ChatRequest, ChatResponse, LlmExecutor};
pub use permission::{GateOutcome, PermissionDecision, PermissionPolicy, ToolGateHook};
pub use plugin::{
    CapabilityBound, Contributions, PhaseHook, PhaseHookPoint, Plugin, PluginManifest,
    ResolvedExecutionEnv, RunEndContext, RunEndDecision, RunEndGuard,
};
// The conversation/id types surfaced through this crate's own ports (e.g.
// `RunEndContext.conversation: &[Message]`). Re-exported so an extension that
// consumes those ports names them here, without a direct `agent-contract` edge.
pub use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
pub use awaken_agent_contract::agent::run::Id as RunId;
pub use resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
pub use resolver::{AgentSnapshotResolver, RunResolver};
pub use resume::{ResumeCommand, ResumeError, ResumeResult, validate_resume};
pub use runnable::{RunnableConfig, RunnableConfigBuilder};
pub use runtime_context::RuntimeRunContext;
pub use snapshot::{ExecutableAgentSnapshot, ExecutableAgentSnapshotId};
pub use tool::{RawTool, Tool, ToolExecutor, ToolOutput};
