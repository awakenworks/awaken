//! Role-owned store groups used by the process composition root.
//!
//! These types make acquisition authority explicit: a split process receives
//! either Control or Coordinator stores, while AllInOne receives both canonical
//! groups.

use std::sync::Arc;

use crate::config;

pub(super) struct ProcessStores {
    /// Durable installation root used to persist the platform Workspace id.
    pub(super) workspace_root: Option<std::path::PathBuf>,
    pub(super) control: Option<ControlStores>,
    pub(super) coordinator: Option<CoordinatorStores>,
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
    /// Control-owned subject aggregate and consent facts.
    pub(super) data_subjects: Arc<dyn awaken_data_subject::DataSubjectRepo>,
    /// Durable Control erasure process checkpoints over the same adapter.
    pub(super) erasure_jobs: Arc<dyn awaken_data_subject::ErasureJobRepo>,
    pub(super) environments: Arc<dyn awaken_environment_contract::EnvRegistry>,
    pub(super) sandbox_policies: Arc<dyn awaken_provisioning_contract::SandboxExecutionPolicyStore>,
}

pub(super) struct CoordinatorStores {
    /// Resources is a sibling component. Coordinator mounts its ports but never
    /// opens or receives Control's Resource-authoring/admin store.
    pub(super) resource_component: awaken_resource_contract::ResourceComponent,
    /// Durable home for the Managed Session aggregate and lifecycle outbox.
    pub(super) sessions: Arc<dyn awaken_session_contract::ManagedSessionRepository>,
    /// Coordinator-owned Deployment and DeploymentRun view over the same physical
    /// repository as Session.
    pub(super) deployments: Arc<dyn awaken_deployment_contract::DeploymentRepository>,
    /// The same physical Session store viewed through the Dream repository interface.
    pub(super) dream_repository: Arc<dyn awaken_ext_memory::DreamRepository>,
    /// Same Session application repository viewed through the extraction-work
    /// interface; kept separate from MemoryRepository and IAM.
    pub(super) memory_extractions: Arc<dyn awaken_ext_memory::MemoryExtractionRepository>,
    /// Coordinator-owned subject-tagged content write port.
    pub(super) capture_sink: Arc<dyn awaken_runtime_contract::CaptureSink>,
    /// A second view of the exact same captured-content adapter for Control's
    /// authenticated erasure command.
    pub(super) captured_content_eraser: Arc<dyn awaken_runtime_contract::ContentEraser>,
    pub(super) environment_work: Arc<dyn awaken_session_contract::work_queue::WorkQueue>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PostgresSchemaMode {
    Migrate,
    Verify,
}

/// One independently owned schema group selected by the deployment role. The
/// list is also the order in which AllInOne composes the same canonical groups.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MigrationComponent {
    Control,
    ControlDataSubject,
    Coordinator,
    CoordinatorCapturedContent,
    Resources,
    ExecutableAgentCatalog,
    ExecutableEnvironmentCatalog,
}

const ALL_IN_ONE_MIGRATIONS: &[MigrationComponent] = &[
    MigrationComponent::Control,
    MigrationComponent::ControlDataSubject,
    MigrationComponent::Coordinator,
    MigrationComponent::CoordinatorCapturedContent,
    MigrationComponent::Resources,
];
const CONTROL_MIGRATIONS: &[MigrationComponent] = &[
    MigrationComponent::Control,
    MigrationComponent::ControlDataSubject,
];
const COORDINATOR_MIGRATIONS: &[MigrationComponent] = &[
    MigrationComponent::Coordinator,
    MigrationComponent::CoordinatorCapturedContent,
    MigrationComponent::Resources,
    MigrationComponent::ExecutableAgentCatalog,
    MigrationComponent::ExecutableEnvironmentCatalog,
];
const WORKER_MIGRATIONS: &[MigrationComponent] = &[];

pub(super) fn migration_manifest(role: config::Role) -> &'static [MigrationComponent] {
    match role {
        config::Role::AllInOne => ALL_IN_ONE_MIGRATIONS,
        config::Role::Control => CONTROL_MIGRATIONS,
        config::Role::Coordinator => COORDINATOR_MIGRATIONS,
        config::Role::Worker => WORKER_MIGRATIONS,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_manifest_follows_bounded_context_ownership() {
        // Cause/effect decision table:
        // R1 Control -> Control schemas only.
        // R2 Coordinator -> Coordinator + co-deployed Resources + its durable
        // executable Agent and Environment projections.
        // R3 Worker -> no authority schema and therefore no database access.
        // R4 AllInOne -> canonical Control, Coordinator, and Resources groups,
        // but no second durable executable projection implementation.
        assert_eq!(
            migration_manifest(config::Role::Control),
            &[
                MigrationComponent::Control,
                MigrationComponent::ControlDataSubject,
            ],
            "R1"
        );
        assert_eq!(
            migration_manifest(config::Role::Coordinator),
            &[
                MigrationComponent::Coordinator,
                MigrationComponent::CoordinatorCapturedContent,
                MigrationComponent::Resources,
                MigrationComponent::ExecutableAgentCatalog,
                MigrationComponent::ExecutableEnvironmentCatalog,
            ],
            "R2"
        );
        assert!(migration_manifest(config::Role::Worker).is_empty(), "R3");
        assert_eq!(
            migration_manifest(config::Role::AllInOne),
            &[
                MigrationComponent::Control,
                MigrationComponent::ControlDataSubject,
                MigrationComponent::Coordinator,
                MigrationComponent::CoordinatorCapturedContent,
                MigrationComponent::Resources,
            ],
            "R4"
        );
    }
}
