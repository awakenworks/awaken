//! Agent configuration aggregate, compilation rules, and repository ports.
//!
//! Durable SQLite/Postgres implementations live in `awaken-config-store`; this
//! crate owns the backend-free Control-domain vocabulary consumed by application
//! services.

#![forbid(unsafe_code)]

mod agent_inputs;
mod compile;
mod config;
/// Reusable MCP authoring definitions; execution remains Agent/Session-owned.
pub mod mcp_config;
/// Account-owned selection preferences, separate from definitions and authority.
pub mod personal_selection;
mod store;

pub use agent_inputs::{AgentEnvironmentBinding, AgentInputConfig};
pub use awaken_agent_contract::ScopeId;
pub use awaken_agent_contract::{AgentSkillBinding, ModelTarget};
pub use awaken_runtime_contract::{ExecutableAgentSnapshot, ExecutableAgentSnapshotBuilder};
pub use compile::{CompileError, compile_published, compile_resolved};
pub use config::{
    AgentConfig, AgentKind, AgentLifecycle, CompactionStrategy, ModelSelection, MultiagentConfig,
    MultiagentTarget, ToolOverride,
};
pub use store::{
    AgentConfigRevision, AuditedConfigWrite, ConfigRegistry, ConfigStoreError, ConfigWrite,
    DEFAULT_SCOPE, ManagementAuditAdmission, ManagementAuditEntry, ManagementAuditRecord,
    ManagementEffect, ManagementEffectReceipt, PublicationRevisionDecision, PublicationState,
    PublicationWrite, ScopedConfig, ScopedConfigRegistry, StoredPublication,
    publication_revision_decision,
};

pub mod catalog_snapshot;
