#![warn(unreachable_pub)]
//! Managed Agents versioned tool DTOs and their neutral application mapping.
mod compilation;
mod dto;
#[cfg(test)]
use awaken_agent_contract::ToolsetPolicy;
#[cfg(test)]
use awaken_session_configuration_contract::{
    BuiltinToolsetMember as AgentToolsetMember, agent_toolset_members,
};
#[cfg(test)]
use compilation::managed_toolset_default_policy;
pub use compilation::{ManagedToolCompileError, compile_agent_tools, resolved_toolsets};
pub use dto::*;
#[cfg(test)]
include!("tests.rs");
