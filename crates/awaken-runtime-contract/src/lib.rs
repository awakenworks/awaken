//! Runtime-facing contract: activation data, snapshot execution, and narrow ports.

pub mod activation;
pub mod capability;
pub mod catalog;
pub mod control;
pub mod execution;
pub mod llm;
pub mod permission;
pub mod plugin_config;
pub mod resolved;
pub mod resolver;
pub mod runtime_context;
pub mod snapshot;
pub mod snapshot_exec;
pub mod tool;

pub use activation::RunActivation;
pub use catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
pub use control::LiveRunControl;
pub use execution::{RunExecutor, RunOutcome};
pub use llm::{ChatRequest, ChatResponse, LlmExecutor};
pub use permission::{GateOutcome, PermissionDecision, PermissionPolicy, ToolGateHook};
pub use resolved::{CatalogFingerprint, ResolvedSpec};
pub use resolver::{AgentSnapshotResolver, RunResolver};
pub use runtime_context::RuntimeRunContext;
pub use snapshot::{ExecutableAgentSnapshot, ExecutableAgentSnapshotId};
pub use snapshot_exec::{RunWithSnapshotCommand, RunWithSnapshotExecutor};
pub use tool::{RawTool, Tool, ToolExecutor, ToolOutput};
