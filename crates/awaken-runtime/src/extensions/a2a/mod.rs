//! Sub-agent delegation tools.
//!
//! - `AgentTool`: unified tool that delegates to local or remote agents.
//! - `AgentBackend`: trait for delegation backends (local, A2A, etc.).

pub(crate) mod a2a_backend;
mod agent_tool;
mod backend;
mod local_backend;
mod progress_sink;

pub use a2a_backend::A2aConfig;
pub use agent_tool::AgentTool;
pub use backend::{AgentBackend, AgentBackendError, DelegateRunResult, DelegateRunStatus};
pub use local_backend::LocalBackend;

#[cfg(test)]
mod tests;
