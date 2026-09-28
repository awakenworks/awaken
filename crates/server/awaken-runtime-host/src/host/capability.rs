//! The capability groups a [`SharedHost`] owns beside its CORE fields.
//!
//! Each group is a plain owned struct defined in its own module together with
//! the `SharedHost::with_*` installers that write it, so construction and the
//! modules that read a capability name exactly one group. Cross-group Host
//! behaviour stays in the Host's own modules.

mod agent;
mod capture;
mod dispatch;
mod environment;
mod execution;
mod mcp;
mod resources;
mod tools;
mod worker;

pub(crate) use agent::AgentExecution;
pub(crate) use capture::ContentCaptureCapability;
pub(crate) use dispatch::DispatchCapability;
pub use dispatch::DispatchPoolSizing;
pub use environment::ContainerEnvironmentComponents;
pub(crate) use environment::EnvironmentProviders;
pub(crate) use execution::AttemptBackends;
pub(crate) use mcp::McpRelayCapability;
pub(crate) use resources::ResourcePorts;
pub(super) use resources::{ResourceInputs, WorkerContentAdapters};
pub(crate) use tools::ToolOffering;
pub(crate) use worker::WorkerTransports;
