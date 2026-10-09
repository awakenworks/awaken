//! The capability groups a [`SharedHost`] owns beside its CORE fields.
//!
//! Each group is a plain owned struct defined in its own module, so the
//! modules that read a capability name exactly one group. A composition
//! selects every group's dependencies on [`crate::HostDependencies`] before the
//! Host exists; groups that also carry Host runtime state (Agent execution,
//! Resources, dispatch) have a separate selection type that the one Host
//! constructor turns into the group. Cross-group Host behaviour stays in the
//! Host's own modules.

mod agent;
mod capture;
mod dispatch;
mod environment;
mod execution;
mod execution_topology;
mod resources;
mod tools;
mod worker;

pub(crate) use agent::{AgentExecution, AgentSelection};
pub(crate) use capture::ContentCaptureCapability;
pub(crate) use dispatch::DispatchCapability;
pub(crate) use environment::EnvironmentProviders;
pub use environment::{TrustedHostPolicy, WarmPool};
pub(crate) use execution::AttemptBackends;
pub use execution_topology::{
    DispatchPoolSizing, ExecutionTopology, LeaseOwnerId, LocalExecution, QueueDepthSource,
    RunIngress,
};
#[cfg(test)]
pub(super) use resources::local_memory_extraction_repository;
#[cfg(any(test, feature = "test-support"))]
pub use resources::{LocalResourcePorts, LocalSkillPorts};
pub(super) use resources::{ResourceInputs, WorkerContentAdapters};
pub(crate) use resources::{ResourcePorts, ResourceSelection};
pub(crate) use tools::ToolOffering;
pub(crate) use worker::WorkerTransports;
