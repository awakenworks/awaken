//! Sub-agent delegation tools.
//!
//! - `AgentTool`: unified tool that delegates to local or remote agents.
//! - `AgentBackend`: trait for delegation backends (local, A2A, etc.).

pub(crate) mod a2a_backend;
mod agent_tool;
mod backend;
mod local_backend;
mod progress_sink;
pub(crate) mod scheduled_backend;

pub use a2a_backend::{A2aBackendFactory, A2aConfig};
pub use agent_tool::AgentTool;
pub use backend::{
    AgentBackend, AgentBackendError, AgentBackendFactory, AgentBackendFactoryError,
    DelegateRunResult, DelegateRunStatus,
};
pub use local_backend::LocalBackend;
pub use scheduled_backend::{ScheduledBackendFactory, ScheduledConfig};

#[cfg(test)]
mod tests;
