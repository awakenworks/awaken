//! The config-authoring plane (ADR-0036/slice A).
//!
//! Extracted from the data-plane host (`awaken-runtime-host`) so both planes can
//! share it without the authoring/authz plane (`awaken-control`) depending on the
//! execution host — preserving the **control ⊥ execution** invariant. This crate is
//! pure config-plane logic: the config service and its CRUD router, the
//! advertised-capabilities router, the model-binding resolver, and the scoped tool
//! catalog. It references neither `SharedHost` nor run execution.
//!
//! Composition roots and `awaken-control` depend on this authoritative owner
//! directly; the Runtime Host does not act as a public facade.

mod agent_projection;
mod binding_resolver;
mod capabilities;
mod compaction;
mod config_plane;
mod config_routes;
mod config_service;
mod credential_reference;
mod managed_agent;
mod managed_model_id;
mod plugin_validation;
mod publication;
mod registration_reconciliation;
mod service_access;
mod service_wiring;
mod tool_catalog;
mod web_search_publication;

pub use binding_resolver::{
    ConfigServiceReconciler, ModelPublicationResolver, PublicationBindingReconciler,
    PublicationResolutionError, ResolvedPublicationModels,
};
pub use capabilities::{
    LocalRuntimeCapability, RuntimeCapability, RuntimeCapabilitySource, capabilities_router,
    capabilities_router_with_source, sandbox_execution_policy_capability,
    static_runtime_capabilities,
};
pub use config_plane::ConfigPlane;
pub use config_routes::config_router;
pub use config_service::ConfigService;
pub use credential_reference::CredentialReferenceValidator;
pub use managed_agent::{agent_config_from_managed, managed_from_agent_config};
pub use managed_model_id::{ManagedModelIdError, parse_managed_model_id, render_managed_model_id};
pub use plugin_validation::PluginPublicationResolver;
pub use publication::{PublishError, ValidationIssue};
pub use tool_catalog::{
    RESERVED_ADMIN_SCOPE, ScopedToolCatalog, StaticToolCatalog, ToolCatalogSource,
};
pub use web_search_publication::WebSearchPublicationResolver;
