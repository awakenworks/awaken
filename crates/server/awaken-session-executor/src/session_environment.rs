//! The single live execution environment owned by a Session.
//!
//! Host code depends on this capability object instead of retaining a concrete
//! sandbox in `SessionCtx`. Workdir is the first adapter; Namespace and Container
//! plug into the same owner without adding another Native/ACP lifecycle.

mod agent_sandbox;
mod container_files;
mod container_repositories;
mod container_skills;
mod environment;
mod lifecycle;
mod provider;
mod repository_realizer;
mod session_files;
mod session_hand;
pub(crate) use agent_sandbox::AgentSandbox;
pub(crate) use environment::SessionEnvironment;
pub(crate) use provider::SessionEnvironmentProvider;
pub use session_hand::HandExecutorFactory;
#[cfg(test)]
pub(crate) use session_hand::UnusedHandExecutorFactory;

#[cfg(test)]
mod tests;
