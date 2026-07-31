//! Role-owned store groups used by the process composition root.
//!
//! These types make acquisition authority explicit: a split process receives
//! either Control or Coordinator stores, while AllInOne receives both canonical
//! groups. Environment remains the documented transitional shared registry.

use std::sync::Arc;

use crate::config;

pub(super) struct ProcessStores {
    /// Durable installation root used to persist the platform Workspace id.
    pub(super) workspace_root: Option<std::path::PathBuf>,
    pub(super) control: Option<ControlStores>,
    pub(super) coordinator: Option<CoordinatorStores>,
    pub(super) environments: Arc<awaken_protocol_managed::EnvironmentState>,
}

pub(super) struct ControlStores {
    pub(super) catalog: Arc<dyn awaken_model_catalog::repo::CatalogRepo>,
    pub(super) credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
    pub(super) secrets: Arc<dyn awaken_credential_vault::SecretStore>,
    pub(super) profiles: Arc<dyn awaken_admin_config_api::InferenceProfileStore>,
    pub(super) resources: Arc<dyn awaken_config_resolver::AgentInputBindingRepository>,
    /// Authored webhook endpoints are an id-addressed config resource beside
    /// profiles/MCP: one admin store implements these distinct Control ports.
    pub(super) webhooks: Arc<dyn awaken_admin_config_api::WebhookStore>,
    /// Rich Agent drafts and immutable publications, scoped per Workspace.
    pub(super) config: Arc<dyn awaken_config_store::ScopedConfigRegistry>,
}

pub(super) struct CoordinatorStores {
    /// Resources is a sibling component. Coordinator mounts its ports but never
    /// opens or receives Control's Resource-authoring/admin store.
    pub(super) resource_component: awaken_resource_contract::ResourceComponent,
    /// Durable home for the Managed Session aggregate and lifecycle outbox.
    pub(super) sessions: Arc<dyn awaken_session_contract::ManagedSessionRepository>,
    /// Coordinator-owned Deployment and DeploymentRun view over the same physical
    /// repository as Session.
    pub(super) deployments: Arc<dyn awaken_protocol_managed::DeploymentRepository>,
    /// The same physical Session store viewed through the Dream repository interface.
    pub(super) dream_repository: Arc<dyn awaken_protocol_managed::DreamRepository>,
    /// Same Session application repository viewed through the extraction-work
    /// interface; kept separate from MemoryRepository and IAM.
    pub(super) memory_extractions: Arc<dyn awaken_protocol_managed::MemoryExtractionRepository>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PostgresSchemaMode {
    Migrate,
    Verify,
}

pub(super) fn role_owns_control_component(role: config::Role) -> bool {
    matches!(role, config::Role::AllInOne | config::Role::Control)
}

pub(super) fn role_owns_managed_execution(role: config::Role) -> bool {
    matches!(role, config::Role::AllInOne | config::Role::Coordinator)
}

#[cfg(test)]
pub(super) fn role_composes_resource_component(role: config::Role) -> bool {
    role_owns_managed_execution(role)
}
