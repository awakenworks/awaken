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
mod config_routes;
mod credential_reference;
mod installed_catalog;
mod managed_agent;
mod plugin_validation;
mod publication;
mod runtime_snapshot_source;
mod service_access;
mod service_wiring;
mod tool_catalog;
mod warm_install;

pub use agent_projection::ConfigServiceAgentSource;
pub use binding_resolver::{
    ConfigServiceReconciler, ModelPublicationResolver, PublicationBindingReconciler,
    PublicationResolutionError, ResolvedPublicationModels,
};
pub use capabilities::{
    LocalRuntimeCapability, RuntimeCapability, RuntimeCapabilitySource, capabilities_router,
    capabilities_router_with_source, sandbox_execution_policy_capability,
    static_runtime_capabilities,
};
pub use config_plane::{ConfigPlane, ConfigService};
pub use config_routes::config_router;
pub use credential_reference::CredentialReferenceValidator;
pub use managed_agent::{agent_config_from_managed, managed_from_agent_config};
pub use plugin_validation::PluginPublicationResolver;
pub use publication::{PublishError, ValidationIssue};
pub use tool_catalog::{
    RESERVED_ADMIN_SCOPE, ScopedToolCatalog, StaticToolCatalog, ToolCatalogSource,
};
