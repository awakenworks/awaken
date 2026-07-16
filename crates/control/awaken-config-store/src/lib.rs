//! The config domain's store: compile a declarative agent config into a
//! content-addressed publication the runtime installs, and persist configs and
//! publications durably under the `config` table namespace (ADR-0031).
//!
//! The config domain and the runtime are separate bounded contexts; their only
//! seam is the published `ExecutableAgentSnapshot` / `RuntimeCatalogInstall`. The
//! config store produces them; the runtime validates and installs them and never
//! edits config records.

#![forbid(unsafe_code)]

mod compile;
mod config;
mod schema;
mod store;

mod postgres;
mod sqlite;

pub use awaken_runtime_contract::runnable::{RunnableConfig, RunnableConfigBuilder};
pub use awaken_tenancy::ScopeId;
pub use compile::{CompileError, compile, compile_with_resource_prompts, compose_instructions};
pub use config::{AgentConfig, CompactionStrategy, ModelSelection, ToolOverride};
pub use postgres::{PostgresConfigStore, StoreError as PostgresStoreError};
pub use schema::config_bundle;
pub use sqlite::{SqliteConfigStore, StoreError as SqliteStoreError};
pub use store::{
    ConfigRegistry, ConfigStoreError, DEFAULT_SCOPE, PublicationState, ScopedConfig,
    ScopedConfigRegistry, StoredPublication,
};
