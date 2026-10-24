//! The sandbox-backed Session environment: the one live execution
//! environment a Session owns, realized on the Workdir, Namespace or container
//! tier, and the provider that realizes, adopts and restores it.
//!
//! [`SandboxEnvironment`] and [`SandboxEnvironmentProvider`] play every role
//! of `awaken-session-environment-contract`; the process composition hands
//! them to the Runtime Host as role handles
//! ([`SandboxEnvironment::into_session_environment`],
//! [`SandboxEnvironmentProvider::into_session_provider`]). The Runtime Host
//! never links this crate outside its own tests.

mod agent_sandbox;
mod container_files;
mod container_repositories;
mod container_skills;
mod environment;
mod lifecycle;
mod provider;
mod repository_realizer;
mod roles;
mod session_files;
mod session_hand;

pub use container_skills::ContainerSkillCache;
pub use environment::{ContainerProviderSource, SandboxEnvironment};
pub use provider::{SandboxEnvironmentProvider, WorkdirHandConfig};
pub use session_hand::SessionHandExecutor;

use agent_sandbox::{AgentFilesystemClient, AgentFilesystemView};
use awaken_session_environment_contract::{HandExecutorFactory, SessionEnvironment};

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

#[cfg(test)]
mod tests;
