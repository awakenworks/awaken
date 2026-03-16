//! Plugin orchestration, sub-agent management, and lifecycle composition for AgentOS.
//!
//! - [`composition`]: agent definitions, builder, registries, and wiring.
//! - [`runtime`]: run preparation, execution, stop policies, and background tasks.
//! - [`extensions`]: feature-gated bridges to permission, reminder, skills, observability, and MCP.
#![allow(missing_docs)]

pub use tirea_contract as contracts;

pub mod composition;
pub mod engine;
pub mod extensions;
pub mod runtime;
pub mod team_tools;

// ── Top-level re-exports for common entry points ────────────────────────

pub use composition::{AgentDefinition, AgentOsBuilder, RegistrySet, ToolBehaviorBundle};
pub use runtime::{AgentOs, AgentOsRunError, PreparedRun, RunStream};
pub use team_tools::{TeamPlugin, TeamStores, TeamWiring};
