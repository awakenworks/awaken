//! Sub-agent delegation tools.
//!
//! - `AgentTool`: unified tool that delegates to local or remote agents.
//! - `ExecutionBackend`: trait for execution backends (re-exported for
//!   compatibility from `awaken_runtime::backend`).

pub(crate) mod a2a_backend;
mod agent_tool;
mod progress_sink;

pub use crate::backend::{
    BackendRunResult as DelegateRunResult, BackendRunStatus as DelegateRunStatus,
    ExecutionBackend as AgentBackend, ExecutionBackendError as AgentBackendError,
    ExecutionBackendFactory as AgentBackendFactory,
    ExecutionBackendFactoryError as AgentBackendFactoryError, LocalBackend,
};
pub use a2a_backend::{A2aBackendFactory, A2aConfig};
pub use agent_tool::AgentTool;

#[cfg(test)]
mod tests;
