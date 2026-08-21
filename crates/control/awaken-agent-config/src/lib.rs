//! Agent configuration aggregate, compilation rules, and repository ports.
//!
//! Durable SQLite/Postgres implementations live in `awaken-config-store`; this
//! crate owns the backend-free Control-domain vocabulary consumed by application
//! services.

#![forbid(unsafe_code)]

mod agent_inputs;
mod compile;
mod config;
mod store;

pub use agent_inputs::{AgentEnvironmentBinding, AgentInputConfig};
pub use awaken_agent_contract::{AgentSkillBinding, ModelTarget};
pub use awaken_runtime_contract::{ExecutableAgentSnapshot, ExecutableAgentSnapshotBuilder};
pub use awaken_tenancy::ScopeId;
pub use compile::{CompileError, compile_published, compile_resolved};
pub use config::{
    AgentConfig, AgentKind, AgentLifecycle, CompactionStrategy, ModelSelection, MultiagentConfig,
    MultiagentTarget, ToolOverride,
};
pub use store::{
    AgentConfigRevision, AuditedConfigWrite, ConfigRegistry, ConfigStoreError, ConfigWrite,
    DEFAULT_SCOPE, ManagementAuditEntry, ManagementAuditRecord, ManagementEffect,
    PublicationRevisionDecision, PublicationState, ScopedConfig, ScopedConfigRegistry,
    StoredPublication, publication_revision_decision,
};
