//! Role-owned store groups used by product-process startup.
//!
//! These types make acquisition authority explicit: a split process receives
//! either Control or Coordinator stores, while AllInOne receives both canonical
//! groups.

use std::sync::Arc;

use awaken_service_lifecycle::{StartupComponent, StartupRole, startup_requires};

use crate::config;

pub(super) struct ProcessStores {
    /// Durable installation root used to persist the platform Workspace id.
    pub(super) workspace_root: Option<std::path::PathBuf>,
    /// The one installation Workspace coordinate resolved by store startup and
    /// passed unchanged to IAM, Control, and Coordinator adapters.
    pub(super) platform_workspace: String,
    pub(super) control: Option<ControlStores>,
    pub(super) coordinator: Option<CoordinatorStores>,
}

pub(super) struct ControlStores {
    pub(super) catalog: Arc<dyn awaken_model_catalog::repo::CatalogRepo>,
    pub(super) credentials: Arc<dyn awaken_credential_vault::repo::ManagedCredentialRepository>,
    pub(super) secrets: Arc<dyn awaken_credential_vault::SecretStore>,
    pub(super) profiles: Arc<dyn awaken_config_resolver::InferenceProfileStore>,
    pub(super) resources: Arc<dyn awaken_config_resolver::AgentInputBindingRepository>,
    /// Authored webhook endpoints are an id-addressed config resource beside
    /// profiles/MCP: one admin store implements these distinct Control ports.
    pub(super) webhooks: Arc<dyn awaken_config_resolver::WebhookStore>,
    /// Rich Agent drafts and immutable publications, scoped per Workspace.
    pub(super) config: Arc<dyn awaken_agent_config::ScopedConfigRegistry>,
    /// Control-owned subject aggregate and consent facts.
    pub(super) data_subjects: Arc<dyn awaken_data_subject_application::DataSubjectRepo>,
    /// Durable Control erasure process checkpoints over the same adapter.
    pub(super) erasure_jobs: Arc<dyn awaken_data_subject_application::ErasureJobRepo>,
    pub(super) environments: Arc<dyn awaken_environment_contract::EnvRegistry>,
    pub(super) sandbox_policies: Arc<dyn awaken_provisioning_contract::SandboxExecutionPolicyStore>,
}

pub(super) struct CoordinatorStores {
    /// Resources is a sibling component. Coordinator mounts its ports but never
    /// opens or receives Control's Resource-authoring/admin store.
    pub(super) resources: awaken_resource_application::ResourcesApplication,
    /// Durable home for the Managed Session aggregate and lifecycle outbox.
    pub(super) sessions: Arc<dyn awaken_session_contract::ManagedSessionRepository>,
    /// Coordinator-owned short-lived application capabilities. It shares the
    /// selected Session database but has its own aggregate table and ledger.
    pub(super) application_access:
        Arc<awaken_coordinator::application_access_store::ApplicationAccessStore>,
    /// Coordinator-owned Deployment and DeploymentRun view over the same physical
    /// repository as Session.
    pub(super) deployments: Arc<dyn awaken_deployment_contract::DeploymentRepository>,
    /// The same physical Session store viewed through the Dream process-store port.
    pub(super) dream_process_store: Arc<dyn awaken_session_contract::DreamProcessStore>,
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

/// Every typed port backed by the one canonical Managed Session repository.
/// Process assembly retains these views and never opens the shared store twice.
pub(super) struct SessionStoreViews {
    pub(super) sessions: Arc<dyn awaken_session_contract::ManagedSessionRepository>,
    pub(super) deployments: Arc<dyn awaken_deployment_contract::DeploymentRepository>,
    pub(super) memory_extractions: Arc<dyn awaken_ext_memory::MemoryExtractionRepository>,
    pub(super) dream_process_store: Arc<dyn awaken_session_contract::DreamProcessStore>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PostgresSchemaMode {
    Migrate,
    Verify,
}

pub(super) fn ensure_store_parent(backend: &awaken_control::StoreBackend) -> Result<(), String> {
    if let awaken_control::StoreBackend::Sqlite(path) = backend
        && let Some(parent) = path.parent()
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create store directory {}: {error}", parent.display()))?;
    }
    Ok(())
}

/// Open the shared coordinator database's canonical schema owner before any
/// sibling WorkQueue/ApplicationAccess adapter can create its own ledger.
pub(super) async fn open_session_store_views(
    backend: &awaken_control::StoreBackend,
    postgres_schema: PostgresSchemaMode,
) -> Result<SessionStoreViews, String> {
    use awaken_control::StoreBackend;

    ensure_store_parent(backend)?;
    match backend {
        StoreBackend::Sqlite(path) => {
            let repository = Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open(&path.to_string_lossy())
                    .map_err(|error| format!("open sessions SQLite {}: {error}", path.display()))?,
            );
            Ok(SessionStoreViews {
                sessions: repository.clone(),
                deployments: repository.clone(),
                memory_extractions: repository.clone(),
                dream_process_store: repository,
            })
        }
        StoreBackend::Postgres(url) => {
            let repository = Arc::new(
                match postgres_schema {
                    PostgresSchemaMode::Migrate => {
                        awaken_session_store::PostgresManagedSessionRepository::connect(url).await
                    }
                    PostgresSchemaMode::Verify => {
                        awaken_session_store::PostgresManagedSessionRepository::connect_existing(
                            url,
                        )
                        .await
                    }
                }
                .map_err(|error| format!("connect sessions Postgres: {error}"))?,
            );
            Ok(SessionStoreViews {
                sessions: repository.clone(),
                deployments: repository.clone(),
                memory_extractions: repository.clone(),
                dream_process_store: repository,
            })
        }
    }
}

pub(super) async fn open_environment_work(
    backend: &awaken_control::StoreBackend,
    postgres_schema: PostgresSchemaMode,
) -> Result<Arc<dyn awaken_session_contract::work_queue::WorkQueue>, String> {
    use awaken_control::StoreBackend;

    ensure_store_parent(backend)?;
    match backend {
        StoreBackend::Sqlite(path) => Ok(Arc::new(
            awaken_work_store::SqliteWorkQueue::open(&path.to_string_lossy())
                .map_err(|error| format!("open work queue SQLite: {error}"))?,
        )),
        StoreBackend::Postgres(url) => Ok(Arc::new(
            match postgres_schema {
                PostgresSchemaMode::Migrate => {
                    awaken_work_store::PostgresWorkQueue::connect(url).await
                }
                PostgresSchemaMode::Verify => {
                    awaken_work_store::PostgresWorkQueue::connect_existing(url).await
                }
            }
            .map_err(|error| format!("connect work queue Postgres: {error}"))?,
        )),
    }
}

pub(super) async fn open_application_access(
    backend: &awaken_control::StoreBackend,
    postgres_schema: PostgresSchemaMode,
) -> Result<Arc<awaken_coordinator::application_access_store::ApplicationAccessStore>, String> {
    use awaken_control::StoreBackend;

    let store = match backend {
        StoreBackend::Sqlite(path) => {
            awaken_coordinator::application_access_store::ApplicationAccessStore::open_sqlite(
                &path.to_string_lossy(),
            )
            .await
            .map_err(|error| format!("open application access SQLite: {error}"))?
        }
        StoreBackend::Postgres(url) => match postgres_schema {
            PostgresSchemaMode::Migrate => {
                awaken_coordinator::application_access_store::ApplicationAccessStore::connect_postgres(
                    url,
                )
                .await
            }
            PostgresSchemaMode::Verify => {
                awaken_coordinator::application_access_store::ApplicationAccessStore::connect_existing_postgres(
                    url,
                )
                .await
            }
        }
        .map_err(|error| format!("connect application access Postgres: {error}"))?,
    };
    Ok(Arc::new(store))
}

/// One independently owned schema group selected by the deployment role. The
/// list is also the order in which AllInOne configures the same canonical groups.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MigrationComponent {
    /// Every Control-owned authority, including Data Subject, Environment, and
    /// sandbox policy configuration.
    Control,
    /// Coordinator authorities and its rebuildable executable projections.
    Coordinator,
    /// The co-deployed but independently owned Resources application.
    Resources,
}

const ALL_IN_ONE_MIGRATIONS: &[MigrationComponent] = &[
    MigrationComponent::Control,
    MigrationComponent::Resources,
    MigrationComponent::Coordinator,
];
const CONTROL_MIGRATIONS: &[MigrationComponent] = &[MigrationComponent::Control];
const COORDINATOR_MIGRATIONS: &[MigrationComponent] = &[
    MigrationComponent::Resources,
    MigrationComponent::Coordinator,
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

pub(super) const fn lifecycle_startup_role(role: config::Role) -> Option<StartupRole> {
    match role {
        config::Role::AllInOne => Some(StartupRole::AllInOne),
        config::Role::Control => Some(StartupRole::Control),
        config::Role::Coordinator => Some(StartupRole::Coordinator),
        config::Role::Worker => None,
    }
}

pub(super) fn role_owns_control_component(role: config::Role) -> bool {
    lifecycle_startup_role(role)
        .is_some_and(|role| startup_requires(role, StartupComponent::Control))
}

pub(super) fn role_owns_managed_execution(role: config::Role) -> bool {
    lifecycle_startup_role(role)
        .is_some_and(|role| startup_requires(role, StartupComponent::Coordinator))
}

#[cfg(test)]
pub(super) fn role_hosts_resources(role: config::Role) -> bool {
    lifecycle_startup_role(role)
        .is_some_and(|role| startup_requires(role, StartupComponent::Resources))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_manifest_follows_bounded_context_ownership() {
        // Cause/effect decision table:
        // R1 Control -> one Control migration unit, including every Control
        // authority schema.
        // R2 Coordinator -> Coordinator + co-deployed Resources; executable
        // Agent/Environment projections are internal to the Coordinator unit.
        // R3 Worker -> no authority schema and therefore no database access.
        // R4 AllInOne -> the same three canonical units; local executable
        // projections remain in memory and never become another manifest unit.
        assert_eq!(
            migration_manifest(config::Role::Control),
            &[MigrationComponent::Control],
            "R1"
        );
        assert_eq!(
            migration_manifest(config::Role::Coordinator),
            &[
                MigrationComponent::Resources,
                MigrationComponent::Coordinator,
            ],
            "R2"
        );
        assert!(migration_manifest(config::Role::Worker).is_empty(), "R3");
        assert_eq!(
            migration_manifest(config::Role::AllInOne),
            &[
                MigrationComponent::Control,
                MigrationComponent::Resources,
                MigrationComponent::Coordinator,
            ],
            "R4"
        );
    }
}
