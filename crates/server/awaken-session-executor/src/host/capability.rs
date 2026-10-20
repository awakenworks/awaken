//! The capability groups a [`ExecutorComponents`] owns beside its CORE fields.
//!
//! Each group is a plain owned struct defined in its own module, so the
//! modules that read a capability name exactly one group. A composition
//! selects every group's dependencies on [`crate::SessionExecutorBuilder`] before the
//! Host exists; groups that also carry Host runtime state (Agent execution,
//! Resources) have a separate selection type that the one Host
//! constructor turns into the group. Cross-group Host behaviour stays in the
//! Host's own modules.

mod agent;
mod capture;
mod dispatch;
mod execution;
mod resources;
mod tools;
mod worker;

pub(crate) use crate::environment_providers::EnvironmentProviders;
pub use crate::environment_providers::{TrustedHostPolicy, WarmPool};
pub(crate) use agent::{AgentExecution, AgentSelection};
pub(crate) use capture::ContentCaptureCapability;
pub(crate) use dispatch::local_dispatch_owner;
pub(crate) use execution::AttemptBackends;
pub(super) use resources::ResourceInputs;
pub use resources::WorkerContentAdapters;
#[cfg(any(test, feature = "test-support"))]
pub use resources::{LocalResourcePorts, LocalSkillPorts, test_memory_content};
pub(crate) use resources::{ResourcePorts, ResourceSelection};
pub(crate) use tools::ToolOffering;
pub(crate) use worker::WorkerTransports;
