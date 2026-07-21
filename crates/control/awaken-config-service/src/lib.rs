//! The config-authoring plane (ADR-0036/slice A).
//!
//! Extracted from the data-plane host (`awaken-runtime-host`) so both planes can
//! share it without the authoring/authz plane (`awaken-control`) depending on the
//! execution host — preserving the **control ⊥ execution** invariant. This crate is
//! pure config-plane logic: the config service and its CRUD router, the
//! advertised-capabilities router, the model-binding resolver, and the scoped tool
//! catalog. It references neither `SharedHost` nor run execution.
//!
//! The data-plane host re-exports these types for the single-machine composition
//! root; `awaken-control` depends on this crate directly.

mod agent_projection;
mod binding_resolver;
mod capabilities;
mod compaction;
mod config_plane;
mod installed_catalog;
mod managed_agent;
mod publication;
mod tool_catalog;

pub use agent_projection::ConfigServiceAgentSource;
pub use binding_resolver::{
    AssistantBindingReconciler, ConfigServiceReconciler, ModelResolver, ResolvedModel,
    needs_resolution,
};
pub use capabilities::{capabilities_router, runtime_catalog, sandbox_capability};
pub use config_plane::{ConfigPlane, ConfigService, config_router};
pub use publication::{PublishError, ValidationIssue};
pub use tool_catalog::{
    RESERVED_ADMIN_SCOPE, ScopedToolCatalog, StaticToolCatalog, ToolCatalogSource,
};
