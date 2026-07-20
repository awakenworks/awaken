//! The config domain's store: compile a declarative agent config into a
//! content-addressed executable snapshot, and persist configs and
//! publications durably under the `config` table namespace (ADR-0031).
//!
//! The config domain and the runtime are separate bounded contexts; their only
//! seam is the published `ExecutableAgentSnapshot`. The config store produces it;
//! the runtime validates and executes it and never edits config records.

#![forbid(unsafe_code)]

mod compile;
mod config;
mod schema;
mod store;

mod postgres;
mod sqlite;

pub use awaken_runtime_contract::{ExecutableAgentSnapshot, ExecutableAgentSnapshotBuilder};
pub use awaken_tenancy::ScopeId;
pub use compile::{
    CompileError, compile, compile_resolved, compile_with_resource_prompts, compose_instructions,
};
pub use config::{AgentConfig, CompactionStrategy, ModelSelection, ToolOverride};
pub use postgres::{PostgresConfigStore, StoreError as PostgresStoreError};
pub use schema::config_bundle;
pub use sqlite::{SqliteConfigStore, StoreError as SqliteStoreError};
pub use store::{
    AgentConfigRevision, AuditedConfigWrite, ConfigRegistry, ConfigStoreError, ConfigWrite,
    DEFAULT_SCOPE, ManagementAuditEntry, ManagementAuditRecord, ManagementEffect, PublicationState,
    ScopedConfig, ScopedConfigRegistry, StoredPublication,
};
