//! Plugin orchestration, sub-agent management, and lifecycle composition for AgentOS.
//!
//! - [`composition`]: agent definitions, builder, registries, and wiring.
//! - [`runtime`]: run preparation, execution, stop policies, and background tasks.
#![allow(missing_docs)]

pub use tirea_contract as contracts;

pub mod builtin_tools;
pub mod composition;
pub mod engine;
pub mod runtime;

// ── Top-level re-exports for common entry points ────────────────────────

pub use builtin_tools::{read_file, ReadFileTool};
pub use composition::{AgentDefinition, AgentOsBuilder, RegistrySet, ToolBehaviorBundle};
pub use runtime::{AgentOs, AgentOsRunError, PreparedRun, RunStream};
