//! `awaken-cli` library: the single-machine **composition root**.
//!
//! Control (`awaken-control`) and Coordinator (`awaken-server`) do not depend on
//! each other. This crate is their process composition root: it opens deployment
//! stores, builds shared adapters, asks each owner for its router, and exposes
//! exactly the API selected by `config::Role`. AllInOne merges those same routers;
//! it does not maintain a second implementation.
//!
//! The `awaken` binary ([`main`](../main.rs)) is a thin shell over this library.

mod acp_local_credentials;
mod assistant_selection;
mod brain_admin;
pub mod config;
mod console_assets;
mod control;
mod credential_probe;
mod deployment_session_launch;
mod exact_host_model;
mod executable_agent_registration;
mod identity;
mod observation_reconcile;
mod process_surface;
mod worker_transport_security;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use awaken_protocol_managed::{EnvironmentState, ManagedState, VaultState};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_host::{
    ConfigPlane, ConfigService, ExtMcpProbe, ManagedHost, RESERVED_ADMIN_SCOPE, ScopedToolCatalog,
    SharedHost, ToolCatalogSource,
};
use axum::Router;
use exact_host_model::ExactHostModelPublicationResolver;

pub use crate::brain_admin::{
    DrainController, brain_admin_router, register_active_streams_gauge, with_brain_admin,
    with_connection_metric,
};
pub use acp_local_credentials::{
    AcpLocalCredentialResolver, PreparedLocalAcp, build_configured_worker, local_acp_diagnostics,
    prepare_local_acp, registered_memory_mounter_factory,
};
pub use console_assets::mount as mount_console;
pub use console_assets::mount_with_navigation as mount_console_with_navigation;
pub use control::{
    build_control_assembly, build_control_router, build_control_router_with_publication_resolver,
    build_control_router_with_publication_resolver_and_web_search,
};
use identity::identity_wiring;
// Embedded management-plane IAM (ADR-0042/0043 P1) + the mint spec and bootstrap
// constants a test / operator embedding drives — re-exported from the authoring plane.
pub use awaken_control::{
    ADMIN_TOKEN_FILE, BOOTSTRAP_PRINCIPAL, BOOTSTRAP_WORKSPACE, ManagementAuthz,
    ManagementIdentityMode, RemoteManagementAuthz, TokenSpec, embedded_iam,
};
mod live_runtime_capabilities;
use live_runtime_capabilities::LiveRuntimeCapabilities;

/// The two legal composition modes are deliberately disjoint: production
/// publishes catalog-backed provider candidates and installs their credential
/// materializer; deterministic scenarios publish one exact host executor and do
/// not install a provider materializer.
enum PublicationModelComposition {
    PublishedProviders,
    /// A hosted composition owns provider custody and injects its resolver into
    /// the same Awaken publication pipeline. This variant is legal only for the
    /// control-only surface: the separate hosted Worker owns runtime
    /// materialization.
    HostedPublication {
        resolver: Arc<dyn awaken_runtime_host::ModelPublicationResolver>,
    },
    Host {
        executor: Arc<dyn LlmExecutor>,
        binding: awaken_runtime_contract::resolved::ModelBinding,
    },
}

/// Concrete wiring produced by one legal composition mode. Keeping these four
/// values together prevents a provider resolver from being paired with a host
/// executor or a Host publication from receiving a credential materializer.
struct PublicationModelWiring {
    executor: Arc<dyn LlmExecutor>,
    model_ref: String,
    publication_resolver: Arc<dyn awaken_runtime_host::ModelPublicationResolver>,
    materializer: Option<Arc<dyn awaken_runtime_host::InferenceExecutorMaterializer>>,
}

#[derive(Default)]
struct ProcessAssemblyOptions {
    deployment: Option<awaken_runtime_host::DeploymentConfig>,
    org_id: Option<String>,
    mcp_bearer_token: Option<String>,
    role: config::Role,
    cloud_api_base_url: Option<String>,
    cloud_models_enabled: bool,
    local_acp_observations: Vec<awaken_acp_application::AcpHostObservation>,
    hand_executors: BTreeMap<String, Arc<dyn awaken_runtime_contract::tool::ToolExecutor>>,
    web_search_providers: Option<awaken_runtime_host::WebSearchProviderRegistry>,
    web_search_publication_resolver:
        Option<Arc<dyn awaken_runtime_host::PluginPublicationResolver>>,
    executable_agent_wiring: Option<executable_agent_registration::ExecutableAgentWiring>,
    deployment_session_launch: Option<config::DeploymentSessionLaunchConfig>,
    worker_authenticator: Option<Arc<dyn awaken_runtime_host::WorkerRequestAuthenticator>>,
}

/// Router plus the cleartext local setup handoff printed by the CLI once.
pub struct ProcessAssembly {
    pub router: Router,
    pub local_setup: Option<awaken_control::LocalSetupHandoff>,
}

/// Store adapters opened once by the process composition and injected into their
/// Control, Coordinator, Credential, Resource, and Session owners.
struct DeploymentStores {
    /// Durable installation root used to persist the platform Workspace id.
    workspace_root: Option<std::path::PathBuf>,
    resource_plane: awaken_runtime_host::ResourcePlane,
    catalog: Arc<dyn awaken_model_catalog::repo::CatalogRepo>,
    credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
    secrets: Arc<dyn awaken_credential_vault::SecretStore>,
    profiles: Arc<dyn awaken_admin_config_api::InferenceProfileStore>,
    resources: Arc<dyn awaken_config_resolver::AgentInputBindingRepository>,
    resource_catalog: Arc<dyn awaken_protocol_managed::ResourceCatalog>,
    /// Authored webhook endpoints (ADR-0048), an id-addressed config resource beside
    /// profiles/MCP — the same admin store, a distinct interface.
    webhooks: Arc<dyn awaken_admin_config_api::WebhookStore>,
    /// Durable home for the Managed session aggregate (its own `sessions.db`), so a
    /// rehydrated session reports its real config across a restart / peer process.
    sessions: Arc<dyn awaken_session_contract::ManagedSessionRepository>,
    /// Control-owned Deployment and DeploymentRun view over that same physical
    /// Session repository. Keeping the typed port separately avoids an invalid
    /// trait-object cast and prevents an in-memory Deployment authority.
    deployments: Arc<dyn awaken_protocol_managed::DeploymentRepository>,
    /// The same physical Session store viewed through the Dream repository interface.
    dream_repository: Arc<dyn awaken_protocol_managed::DreamRepository>,
    /// Same Session application repository viewed through the extraction-work
    /// interface; kept separate from MemoryRepository and from IAM.
    memory_extractions: Arc<dyn awaken_protocol_managed::MemoryExtractionRepository>,
    /// The config authoring plane (`config.db`): the rich `AgentConfig` drafts the
    /// management console authors directly, and their publications. Scoped so a
    /// workspace's config is fenced from another's (ADR-0051).
    config: Arc<dyn awaken_config_store::ScopedConfigRegistry>,
    /// Self-hosted environments registry + work queue, durable per deployment mode.
    environments: Arc<awaken_protocol_managed::EnvironmentState>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PostgresSchemaMode {
    Migrate,
    Verify,
}

fn ephemeral_resource_plane() -> awaken_runtime_host::ResourcePlane {
    let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
    awaken_runtime_host::ResourcePlane::new(
        files.clone(),
        files,
        Arc::new(awaken_memory_store::VolatileMemoryRepository::new()),
        Arc::new(awaken_skill_store::InMemorySkillStore::new()),
        Arc::new(
            awaken_resource_store::SqliteResourceStore::in_memory()
                .expect("open ephemeral resource lifecycle sqlite"),
        ),
    )
}

async fn open_resource_plane(
    backend: config::ResourcePlaneStoreBackend,
    postgres_schema: PostgresSchemaMode,
) -> Result<awaken_runtime_host::ResourcePlane, String> {
    match backend {
        config::ResourcePlaneStoreBackend::Embedded(root) => {
            Ok(awaken_server::embedded_resource_plane(&root))
        }
        config::ResourcePlaneStoreBackend::Postgres(url) => {
            let lifecycle = match postgres_schema {
                PostgresSchemaMode::Migrate => {
                    awaken_resource_store::PostgresResourceStore::connect(&url).await
                }
                PostgresSchemaMode::Verify => {
                    awaken_resource_store::PostgresResourceStore::connect_existing(&url).await
                }
            }
            .map_err(|error| format!("connect resource lifecycle Postgres: {error}"))?;
            let files = Arc::new(
                match postgres_schema {
                    PostgresSchemaMode::Migrate => {
                        awaken_file_store::postgres::PgFileStore::connect(&url).await
                    }
                    PostgresSchemaMode::Verify => {
                        awaken_file_store::postgres::PgFileStore::connect_existing(&url).await
                    }
                }
                .map_err(|error| format!("connect resource file Postgres: {error}"))?,
            );
            let memory = match postgres_schema {
                PostgresSchemaMode::Migrate => {
                    awaken_memory_store::PostgresMemoryRepository::connect(&url).await
                }
                PostgresSchemaMode::Verify => {
                    awaken_memory_store::PostgresMemoryRepository::connect_existing(&url).await
                }
            }
            .map_err(|error| format!("connect resource memory Postgres: {error}"))?;
            let skills = match postgres_schema {
                PostgresSchemaMode::Migrate => {
                    awaken_skill_store::PgSkillStore::connect(&url).await
                }
                PostgresSchemaMode::Verify => {
                    awaken_skill_store::PgSkillStore::connect_existing(&url).await
                }
            }
            .map_err(|error| format!("connect resource skill Postgres: {error}"))?;
            Ok(awaken_runtime_host::ResourcePlane::new(
                files.clone(),
                files,
                Arc::new(memory),
                Arc::new(skills),
                Arc::new(lifecycle),
            ))
        }
    }
}

const CREDENTIAL_RECONCILIATION_INTERVAL: Duration = Duration::from_secs(60);

/// Keep retrying interrupted credential mutations after startup. A failed
/// material cleanup leaves its durable intent intact, so the next tick resumes.
fn spawn_credential_mutation_reconciliation(
    secrets: Arc<dyn awaken_credential_vault::SecretStore>,
    credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(CREDENTIAL_RECONCILIATION_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The composition root already performed the first pass synchronously.
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(error) = awaken_credential_vault::repo::recover_credential_mutations(
                secrets.as_ref(),
                credentials.as_ref(),
            )
            .await
            {
                eprintln!("credential mutation reconciliation failed: {error}");
            }
            match awaken_credential_vault::repo::reconcile_credential_inventory(
                secrets.as_ref(),
                credentials.as_ref(),
            )
            .await
            {
                Ok(report) if !report.missing_material.is_empty() => eprintln!(
                    "credential inventory is missing referenced material: {:?}",
                    report.missing_material
                ),
                Err(error) => eprintln!("credential inventory reconciliation failed: {error}"),
                _ => {}
            }
        }
    });
}

/// Ephemeral deployment stores: everything in process memory (dev / e2e default).
fn in_memory_deployment_stores() -> DeploymentStores {
    let sessions = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("open ephemeral managed Session repository"),
    );
    let admin = Arc::new(
        awaken_admin_config_api::SqliteAdminStore::open_in_memory()
            .expect("open ephemeral admin store"),
    );
    DeploymentStores {
        workspace_root: None,
        resource_plane: ephemeral_resource_plane(),
        catalog: Arc::new(awaken_model_catalog::repo::InMemoryCatalogRepo::new()),
        credentials: Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
        secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        profiles: admin.clone(),
        resources: admin.clone(),
        resource_catalog: admin.clone(),
        webhooks: admin,
        sessions: sessions.clone(),
        deployments: sessions.clone(),
        memory_extractions: sessions.clone(),
        dream_repository: sessions,
        config: Arc::new(
            awaken_config_store::SqliteConfigStore::open_in_memory().expect("open config store"),
        ),
        environments: Arc::new(EnvironmentState::new()),
    }
}

/// Keep the Managed Session aggregate durable whenever the runtime itself is
/// durable, even when the rest of the process composition intentionally remains
/// ephemeral. A restarted runtime can only rehydrate a governed Session when its
/// configuration and owner fence survive beside the committed thread facts.
#[cfg(test)]
fn deployment_stores_for_runtime_storage(
    storage_dir: Option<&std::path::Path>,
) -> DeploymentStores {
    let mut stores = in_memory_deployment_stores();
    let Some(dir) = storage_dir else {
        return stores;
    };
    stores.workspace_root = Some(dir.to_path_buf());
    std::fs::create_dir_all(dir).expect("create runtime storage directory");
    let sessions = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open(
            &dir.join("sessions.db").to_string_lossy(),
        )
        .expect("open sessions.db under runtime storage directory"),
    );
    stores.sessions = sessions.clone();
    stores.deployments = sessions.clone();
    stores.memory_extractions = sessions.clone();
    stores.dream_repository = sessions;
    stores
}

/// Open the control-plane stores per the [`ControlStoreConfig`](awaken_control::ControlStoreConfig)
/// — each component on its own database (SQLite file or shared Postgres). This
/// is the one durable store assembly: each store independently honors its
/// typed per-component database binding, so a
/// separate control / server process can share the same per-component databases
/// (Option A, shared-DB).
async fn open_deployment_stores(
    cfg: awaken_control::ControlStoreConfig,
    resource_plane: awaken_runtime_host::ResourcePlane,
    workspace_root: std::path::PathBuf,
    key: &[u8; 32],
    postgres_schema: PostgresSchemaMode,
) -> Result<DeploymentStores, String> {
    use awaken_control::StoreBackend;

    // Create the parent directory for any SQLite path (a bundle dir or a custom path).
    fn ensure_parent(backend: &StoreBackend) -> Result<(), String> {
        if let StoreBackend::Sqlite(path) = backend
            && let Some(parent) = path.parent()
        {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "create control-store directory {}: {error}",
                    parent.display()
                )
            })?;
        }
        Ok(())
    }
    let path = |p: &std::path::Path| p.to_string_lossy().into_owned();

    ensure_parent(&cfg.catalog)?;
    let catalog: Arc<dyn awaken_model_catalog::repo::CatalogRepo> = match &cfg.catalog {
        StoreBackend::Sqlite(p) => Arc::new(
            awaken_model_catalog::SqliteCatalogRepo::open(&path(p))
                .map_err(|error| format!("open catalog SQLite {}: {error}", p.display()))?,
        ),
        StoreBackend::Postgres(url) => Arc::new(
            match postgres_schema {
                PostgresSchemaMode::Migrate => {
                    awaken_model_catalog::PostgresCatalogRepo::connect(url).await
                }
                PostgresSchemaMode::Verify => {
                    awaken_model_catalog::PostgresCatalogRepo::connect_existing(url).await
                }
            }
            .map_err(|error| format!("connect catalog Postgres: {error}"))?,
        ),
    };

    // The credential repo and its sealed-secret blobs share the one credential backend.
    ensure_parent(&cfg.credential)?;
    let (credentials, secrets): (
        Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
        Arc<dyn awaken_credential_vault::SecretStore>,
    ) = match &cfg.credential {
        StoreBackend::Sqlite(p) => {
            let file = path(p);
            let (creds, blobs) = awaken_credential_vault::sqlite::open_migrated_pair(&file)
                .map_err(|error| format!("open credential SQLite {}: {error}", p.display()))?;
            (
                Arc::new(creds),
                Arc::new(awaken_credential_vault::SealedAeadSecretStore::over(
                    key,
                    Arc::new(blobs),
                )),
            )
        }
        StoreBackend::Postgres(url) => {
            let (creds, blobs) = match postgres_schema {
                PostgresSchemaMode::Migrate => {
                    awaken_credential_vault::postgres::connect_migrated_pair(url)
                        .await
                        .map_err(|error| format!("connect credential Postgres: {error}"))?
                }
                PostgresSchemaMode::Verify => {
                    awaken_credential_vault::postgres::connect_existing_pair(url)
                        .await
                        .map_err(|error| format!("connect credential Postgres: {error}"))?
                }
            };
            (
                Arc::new(creds),
                Arc::new(awaken_credential_vault::SealedAeadSecretStore::over(
                    key,
                    Arc::new(blobs),
                )),
            )
        }
    };

    // The admin aggregate backs three interfaces (profiles / MCP / webhooks) off one store.
    ensure_parent(&cfg.admin)?;
    let admin_profiles: Arc<dyn awaken_admin_config_api::InferenceProfileStore>;
    let admin_webhooks: Arc<dyn awaken_admin_config_api::WebhookStore>;
    let admin_resources: Arc<dyn awaken_config_resolver::AgentInputBindingRepository>;
    let admin_catalog: Arc<dyn awaken_protocol_managed::ResourceCatalog>;
    match &cfg.admin {
        StoreBackend::Sqlite(p) => {
            let admin = awaken_admin_config_api::SqliteAdminStore::open(&path(p))
                .map_err(|error| format!("open admin SQLite {}: {error}", p.display()))?;
            admin
                .migrate_legacy_memory_stores()
                .map_err(|error| format!("migrate legacy MemoryStore rows: {error}"))?;
            let admin = Arc::new(admin);
            admin_profiles = admin.clone();
            admin_resources = admin.clone();
            admin_catalog = admin.clone();
            admin_webhooks = admin;
        }
        StoreBackend::Postgres(url) => {
            // `PostgresAdminStore::connect` builds its own runtime and blocks, so it
            // must run off the async worker thread to avoid a nested-runtime panic.
            let url = url.clone();
            let admin = Arc::new(
                tokio::task::spawn_blocking(move || match postgres_schema {
                    PostgresSchemaMode::Migrate => {
                        awaken_admin_config_api::PostgresAdminStore::connect(&url)
                    }
                    PostgresSchemaMode::Verify => {
                        awaken_admin_config_api::PostgresAdminStore::connect_existing(&url)
                    }
                })
                .await
                .map_err(|error| format!("join admin Postgres connection: {error}"))?
                .map_err(|error| format!("connect admin Postgres: {error}"))?,
            );
            if postgres_schema == PostgresSchemaMode::Migrate {
                admin
                    .migrate_legacy_memory_stores()
                    .map_err(|error| format!("migrate legacy MemoryStore rows: {error}"))?;
            }
            admin_profiles = admin.clone();
            admin_resources = admin.clone();
            admin_catalog = admin.clone();
            admin_webhooks = admin;
        }
    }

    ensure_parent(&cfg.sessions)?;
    let sessions: Arc<dyn awaken_session_contract::ManagedSessionRepository>;
    let deployments: Arc<dyn awaken_protocol_managed::DeploymentRepository>;
    let memory_extractions: Arc<dyn awaken_protocol_managed::MemoryExtractionRepository>;
    let dream_repository: Arc<dyn awaken_protocol_managed::DreamRepository>;
    match &cfg.sessions {
        StoreBackend::Sqlite(p) => {
            let repository = Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open(&path(p))
                    .map_err(|error| format!("open sessions SQLite {}: {error}", p.display()))?,
            );
            sessions = repository.clone();
            deployments = repository.clone();
            memory_extractions = repository.clone();
            dream_repository = repository;
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
            sessions = repository.clone();
            deployments = repository.clone();
            memory_extractions = repository.clone();
            dream_repository = repository;
        }
    }

    ensure_parent(&cfg.config)?;
    let config: Arc<dyn awaken_config_store::ScopedConfigRegistry> = match &cfg.config {
        StoreBackend::Sqlite(p) => Arc::new(
            awaken_config_store::SqliteConfigStore::open(&path(p))
                .map_err(|error| format!("open config SQLite {}: {error}", p.display()))?,
        ),
        StoreBackend::Postgres(url) => Arc::new(
            match postgres_schema {
                PostgresSchemaMode::Migrate => {
                    awaken_config_store::PostgresConfigStore::connect(url).await
                }
                PostgresSchemaMode::Verify => {
                    awaken_config_store::PostgresConfigStore::connect_existing(url).await
                }
            }
            .map_err(|error| format!("connect config Postgres: {error}"))?,
        ),
    };

    // Self-hosted env registry + work queue follow the session store's backend kind
    // (their own table namespaces, so a shared DB is fine).
    let environments: Arc<EnvironmentState> = match &cfg.sessions {
        StoreBackend::Sqlite(sp) => Arc::new(
            EnvironmentState::with_stores(
                Arc::new(
                    awaken_runtime_host::SqliteEnvRegistry::open(&path(
                        &sp.with_file_name("environments.db"),
                    ))
                    .map_err(|error| format!("open environments SQLite: {error}"))?,
                ),
                Arc::new(
                    awaken_runtime_host::SqliteWorkQueue::open(&path(
                        &sp.with_file_name("work_queue.db"),
                    ))
                    .map_err(|error| format!("open work queue SQLite: {error}"))?,
                ),
            )
            .with_sandbox_policies(Arc::new(
                awaken_sandbox_policy_store::SqliteSandboxExecutionPolicyStore::open(&path(
                    &sp.with_file_name("sandbox_policies.db"),
                ))
                .map_err(|error| format!("open sandbox policies SQLite: {error}"))?,
            )),
        ),
        StoreBackend::Postgres(url) => {
            let environments = match postgres_schema {
                PostgresSchemaMode::Migrate => {
                    awaken_runtime_host::PostgresEnvRegistry::connect(url).await
                }
                PostgresSchemaMode::Verify => {
                    awaken_runtime_host::PostgresEnvRegistry::connect_existing(url).await
                }
            }
            .map_err(|error| format!("connect environments Postgres: {error}"))?;
            let work = match postgres_schema {
                PostgresSchemaMode::Migrate => {
                    awaken_runtime_host::PostgresWorkQueue::connect(url).await
                }
                PostgresSchemaMode::Verify => {
                    awaken_runtime_host::PostgresWorkQueue::connect_existing(url).await
                }
            }
            .map_err(|error| format!("connect work queue Postgres: {error}"))?;
            let sandbox_policies = match postgres_schema {
                PostgresSchemaMode::Migrate => {
                    awaken_sandbox_policy_store::PostgresSandboxExecutionPolicyStore::connect(url)
                        .await
                }
                PostgresSchemaMode::Verify => {
                    awaken_sandbox_policy_store::PostgresSandboxExecutionPolicyStore::connect_existing(url)
                        .await
                }
            }
            .map_err(|error| format!("connect sandbox policies Postgres: {error}"))?;
            Arc::new(
                EnvironmentState::with_stores(Arc::new(environments), Arc::new(work))
                    .with_sandbox_policies(Arc::new(sandbox_policies)),
            )
        }
    };

    Ok(DeploymentStores {
        workspace_root: Some(workspace_root),
        resource_plane,
        catalog,
        credentials,
        secrets,
        profiles: admin_profiles,
        resources: admin_resources,
        resource_catalog: admin_catalog,
        webhooks: admin_webhooks,
        sessions,
        deployments,
        memory_extractions,
        dream_repository,
        config,
        environments,
    })
}

/// Local SQLite is a backend selection, not a second store assembly.
async fn open_local_deployment_stores(
    dir: &std::path::Path,
    key: &[u8; 32],
) -> Result<DeploymentStores, String> {
    let resource_plane = open_resource_plane(
        config::ResourcePlaneStoreBackend::Embedded(dir.to_path_buf()),
        PostgresSchemaMode::Migrate,
    )
    .await?;
    open_deployment_stores(
        awaken_control::ControlStoreConfig::local(dir),
        resource_plane,
        dir.to_path_buf(),
        key,
        PostgresSchemaMode::Migrate,
    )
    .await
}

/// Build all-in-one from the standard typed deployment configuration.
pub async fn build_all_in_one_router() -> Router {
    build_all_in_one_router_with_composition(PublicationModelComposition::PublishedProviders).await
}

/// Hermetic all-in-one composition for tests and embedders that explicitly want
/// volatile stores. It never consults the standard deployment config path.
pub async fn build_ephemeral_all_in_one_router() -> Router {
    assemble_process_router(
        in_memory_deployment_stores(),
        None,
        None,
        None,
        PublicationModelComposition::PublishedProviders,
        ProcessAssemblyOptions::default(),
        None,
    )
    .await
}

/// Canonical product assembly from the command's one resolved configuration.
pub async fn build_all_in_one_router_with_deployment(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
) -> Result<Router, String> {
    build_all_in_one_assembly(deployment, key)
        .await
        .map(|assembly| assembly.router)
}

pub async fn build_all_in_one_assembly(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
) -> Result<ProcessAssembly, String> {
    build_runtime_process_assembly(deployment, key, config::Role::AllInOne).await
}

/// Coordinator-only process assembly. It reuses the exact same Session, Run,
/// Dispatch, and Worker-coordination construction as all-in-one, while the
/// shared role selector keeps Control routes outside the exposed API.
pub async fn build_coordinator_assembly(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
) -> Result<ProcessAssembly, String> {
    build_runtime_process_assembly(deployment, key, config::Role::Coordinator).await
}

async fn build_runtime_process_assembly(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
    role: config::Role,
) -> Result<ProcessAssembly, String> {
    debug_assert!(matches!(
        role,
        config::Role::AllInOne | config::Role::Coordinator
    ));
    let identity = identity_wiring(
        deployment.identity_mode,
        Some(&deployment.data_dir),
        &deployment.org_id,
        &deployment.iam_workspaces,
        &deployment.cloud_iam,
    )?;
    let postgres_schema = match deployment.mode {
        config::OperatingMode::Local => PostgresSchemaMode::Migrate,
        config::OperatingMode::Server => PostgresSchemaMode::Verify,
    };
    let resource_plane = open_resource_plane(deployment.resources.clone(), postgres_schema).await?;
    let stores = open_deployment_stores(
        deployment.control.clone(),
        resource_plane,
        deployment.data_dir.clone(),
        key,
        postgres_schema,
    )
    .await?;
    let hand_executors =
        awaken_server::placement::connect_declared_hands(&deployment.hand_connections).await?;
    let executable_agent_wiring =
        executable_agent_registration::for_runtime_role(role, deployment, postgres_schema).await?;
    let worker_authenticator = worker_transport_security::authenticator(deployment)?;
    let router = assemble_process_router(
        stores,
        identity.iam,
        identity.remote_iam,
        identity.local_browser_auth,
        PublicationModelComposition::PublishedProviders,
        ProcessAssemblyOptions {
            deployment: Some(deployment.runtime.clone()),
            org_id: Some(deployment.org_id.clone()),
            mcp_bearer_token: deployment.mcp_bearer_token.clone(),
            role,
            cloud_api_base_url: Some(deployment.cloud_iam.inference_base_url.clone()),
            cloud_models_enabled: deployment.cloud_models.is_enabled(),
            local_acp_observations: deployment.local_acp_observations.clone(),
            hand_executors,
            web_search_providers: None,
            web_search_publication_resolver: None,
            executable_agent_wiring: Some(executable_agent_wiring),
            deployment_session_launch: Some(deployment.deployment_session_launch.clone()),
            worker_authenticator: Some(worker_authenticator),
        },
        None,
    )
    .await;
    Ok(ProcessAssembly {
        router,
        local_setup: identity.local_setup,
    })
}

/// Explicit deployment migration phase for every management-owned store.
/// Local SQLite startup retains its existing auto-migration behavior; managed
/// PostgreSQL deployments invoke this command before starting application Pods.
pub async fn migrate_deployment_schema(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
) -> Result<(), String> {
    let resource_plane =
        open_resource_plane(deployment.resources.clone(), PostgresSchemaMode::Migrate).await?;
    open_deployment_stores(
        deployment.control.clone(),
        resource_plane,
        deployment.data_dir.clone(),
        key,
        PostgresSchemaMode::Migrate,
    )
    .await
    .map(drop)?;
    executable_agent_registration::migrate(deployment).await
}

/// Build the real env-selected management surface with one explicit in-process
/// scenario executor. This is a dev/e2e composition, not a provider fallback:
/// its exact host candidate is published by a dedicated resolver and no
/// credential/provider materializer is installed.
pub async fn build_all_in_one_router_with_scenario_model(
    model: Arc<dyn LlmExecutor>,
    model_ref: String,
) -> Router {
    build_all_in_one_router_with_composition(PublicationModelComposition::Host {
        executor: model,
        binding: awaken_runtime_contract::resolved::ModelBinding::new(
            "default", model_ref, "default",
        ),
    })
    .await
}

async fn build_all_in_one_router_with_composition(
    model_composition: PublicationModelComposition,
) -> Router {
    let deployment = config::ResolvedDeployment::load(config::ConfigOverrides::default())
        .unwrap_or_else(|error| panic!("deployment configuration: {error}"));
    let key = deployment
        .seal_key
        .load_or_create()
        .unwrap_or_else(|error| panic!("control seal key: {error}"));
    let identity = identity_wiring(
        deployment.identity_mode,
        Some(&deployment.data_dir),
        &deployment.org_id,
        &deployment.iam_workspaces,
        &deployment.cloud_iam,
    )
    .unwrap_or_else(|error| panic!("identity configuration: {error}"));
    let postgres_schema = match deployment.mode {
        config::OperatingMode::Local => PostgresSchemaMode::Migrate,
        config::OperatingMode::Server => PostgresSchemaMode::Verify,
    };
    let resource_plane = open_resource_plane(deployment.resources.clone(), postgres_schema)
        .await
        .unwrap_or_else(|error| panic!("open resource stores: {error}"));
    let stores = open_deployment_stores(
        deployment.control.clone(),
        resource_plane,
        deployment.data_dir.clone(),
        &key,
        postgres_schema,
    )
    .await
    .unwrap_or_else(|error| panic!("open deployment stores: {error}"));
    let hand_executors =
        awaken_server::placement::connect_declared_hands(&deployment.hand_connections)
            .await
            .unwrap_or_else(|error| panic!("declared Hand topology: {error}"));
    assemble_process_router(
        stores,
        identity.iam,
        identity.remote_iam,
        identity.local_browser_auth,
        model_composition,
        ProcessAssemblyOptions {
            deployment: Some(deployment.runtime),
            org_id: Some(deployment.org_id),
            mcp_bearer_token: deployment.mcp_bearer_token,
            role: config::Role::AllInOne,
            cloud_api_base_url: Some(deployment.cloud_iam.inference_base_url),
            cloud_models_enabled: deployment.cloud_models.is_enabled(),
            local_acp_observations: deployment.local_acp_observations,
            hand_executors,
            web_search_providers: None,
            web_search_publication_resolver: None,
            executable_agent_wiring: None,
            deployment_session_launch: Some(deployment.deployment_session_launch),
            worker_authenticator: None,
        },
        None,
    )
    .await
}

/// [`build_all_in_one_router_with_model`] plus a last-mile hook on the assembled host
/// (`customize_host`) — the seam a composition root uses to wire a runtime backend the
/// standard process assembly does not provide, e.g. `host.with_acp(executor)` so `acp:*`
/// threads run on an external CLI while the full managed plane (vault + MCP staging +
/// config plane) is still in play. Keeps the ACP executor's crate out of this module.
pub async fn build_all_in_one_router_with_host_customizer(
    model: Arc<dyn LlmExecutor>,
    binding: awaken_runtime_contract::resolved::ModelBinding,
    customize_host: impl FnOnce(SharedHost) -> SharedHost + Send + 'static,
) -> Router {
    assemble_process_router(
        in_memory_deployment_stores(),
        None,
        None,
        None,
        PublicationModelComposition::Host {
            executor: model,
            binding,
        },
        ProcessAssemblyOptions::default(),
        Some(Box::new(customize_host)),
    )
    .await
}

/// Durable counterpart of [`build_all_in_one_router_with_host_customizer`].
///
/// This is an explicit-input composition seam for restart tests and embeddings
/// that need a real external runtime while retaining the same management and
/// resource-plane state across host lifetimes. The sealing key and storage root
/// are supplied by the caller, avoiding process-global environment races.
pub async fn build_durable_all_in_one_router_with_host_customizer(
    dir: &std::path::Path,
    key: &[u8; 32],
    model: Arc<dyn LlmExecutor>,
    binding: awaken_runtime_contract::resolved::ModelBinding,
    customize_host: impl FnOnce(SharedHost) -> SharedHost + Send + 'static,
) -> Router {
    assemble_process_router(
        open_local_deployment_stores(dir, key)
            .await
            .unwrap_or_else(|error| panic!("open local deployment stores: {error}")),
        None,
        None,
        None,
        PublicationModelComposition::Host {
            executor: model,
            binding,
        },
        ProcessAssemblyOptions::default(),
        Some(Box::new(customize_host)),
    )
    .await
}

/// Build the management router over in-memory stores with an explicit host default
/// model injected — a **test-only** seam so an integration test can drive the real
/// management router with a deterministic (mock) model, keeping the mock out of the
/// production assembly.
pub async fn build_all_in_one_router_with_model(
    model: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
) -> Router {
    assemble_process_router(
        in_memory_deployment_stores(),
        None,
        None,
        None,
        PublicationModelComposition::Host {
            executor: model,
            binding: awaken_runtime_contract::resolved::ModelBinding::new(
                "default", model_ref, "genai",
            ),
        },
        ProcessAssemblyOptions::default(),
        None,
    )
    .await
}

/// [`build_all_in_one_router`] with explicit persistence inputs (no environment
/// read): durable all-in-one over `dir`, sealing secrets under `key`.
/// Exposed so a restart test can rebuild a router over one directory across
/// simulated process lifetimes without racing on process-global env vars.
/// No IAM guard — the open (default) all-in-one process.
pub async fn build_durable_all_in_one_router(dir: &std::path::Path, key: &[u8; 32]) -> Router {
    assemble_process_router(
        open_local_deployment_stores(dir, key)
            .await
            .unwrap_or_else(|error| panic!("open local deployment stores: {error}")),
        None,
        None,
        None,
        PublicationModelComposition::PublishedProviders,
        ProcessAssemblyOptions::default(),
        None,
    )
    .await
}

/// [`build_durable_all_in_one_router`] with the embedded IAM guard enabled — the
/// typed self-managed identity composition. Returns the
/// [`ManagementAuthz`] handle too so a test (or an embedding) can mint further
/// workspace tokens against the same policy state.
pub async fn build_secured_all_in_one_router(
    dir: &std::path::Path,
    key: &[u8; 32],
) -> (Router, Arc<ManagementAuthz>) {
    let iam = embedded_iam(dir);
    let router = assemble_process_router(
        open_local_deployment_stores(dir, key)
            .await
            .unwrap_or_else(|error| panic!("open local deployment stores: {error}")),
        Some(iam.clone()),
        None,
        None,
        PublicationModelComposition::PublishedProviders,
        ProcessAssemblyOptions::default(),
        None,
    )
    .await;
    (router, iam)
}

/// Assemble a process over an explicit store set, optionally gated by the
/// embedded IAM guard (`iam`). The authoring / authz half comes from
/// [`awaken_control::control_router`] (guard wraps ONLY admin + vault); the data
/// plane comes from [`awaken_server::mount_with_managed`]; this composition root
/// weaves them and keeps the warm-load + inert no-model placeholder wired here.
async fn assemble_process_router(
    stores: DeploymentStores,
    iam: Option<Arc<ManagementAuthz>>,
    remote_iam: Option<Arc<RemoteManagementAuthz>>,
    local_browser_auth: Option<awaken_control::LocalBrowserAuth>,
    model_composition: PublicationModelComposition,
    assembly: ProcessAssemblyOptions,
    // An optional last-mile hook on the assembled data-plane host, applied before it is
    // shared. The composition root uses it to wire a runtime backend the standard process
    // does not assemble itself (e.g. an ACP executor for `acp:*` threads) without this
    // module naming that backend's crate. `None` in production; `Some` in a scenario that
    // serves external-CLI sessions.
    customize_host: Option<Box<dyn FnOnce(SharedHost) -> SharedHost + Send>>,
) -> Router {
    let role = assembly.role;
    let worker_authenticator = assembly.worker_authenticator.unwrap_or_else(|| {
        Arc::new(awaken_runtime_host::HeaderWorkerAuthenticator)
            as Arc<dyn awaken_runtime_host::WorkerRequestAuthenticator>
    });
    let executable_agent_wiring = assembly
        .executable_agent_wiring
        .unwrap_or_else(executable_agent_registration::ExecutableAgentWiring::local);
    let deployment_session_launch = assembly.deployment_session_launch;
    let deployment = assembly.deployment;
    let hand_executors = assembly.hand_executors;
    let cloud_api_base_url = assembly.cloud_api_base_url;
    let cloud_models_enabled = assembly.cloud_models_enabled;
    let org_id = assembly.org_id.unwrap_or_else(local_org_id);
    let managed_rate_limiter =
        Arc::new(awaken_protocol_managed::ManagedRateLimiter::for_organization(org_id.clone()));
    let mcp_bearer_token = assembly.mcp_bearer_token;
    let DeploymentStores {
        workspace_root,
        resource_plane,
        catalog,
        credentials,
        secrets,
        profiles,
        resources: resource_store,
        resource_catalog,
        webhooks: webhook_store,
        sessions,
        deployments,
        memory_extractions,
        dream_repository,
        config,
        environments,
    } = stores;
    // Cause/effect composition rule: one selected ResourcePlane is moved intact
    // into the Host. The management Skill API borrows the one additional view it
    // needs; no tuple decomposition or parallel ResourcePlane reconstruction.
    let skill_store = resource_plane.skill_store();
    // Resolve the installation's Workspace exactly once, then inject the same
    // coordinate into every adapter assembled below. Durable roots persist it;
    // ephemeral roots receive a process-local generated coordinate.
    let platform_workspace = workspace_root.as_deref().map_or_else(
        SharedHost::provision_local_workspace,
        SharedHost::provision_local_workspace_at,
    );
    let brokered_client = cloud_models_enabled
        .then(|| {
            remote_iam
                .as_ref()
                .and_then(|authz| authz.cloud_user_token())
        })
        .flatten()
        .map(|token| {
            let base_url = cloud_api_base_url
                .clone()
                .expect("Awaken Cloud identity requires a Cloud inference API URL");
            Arc::new(
                awaken_server::brokered_inference::HttpBrokeredInferenceClient::new(
                    base_url,
                    token,
                    platform_workspace.clone(),
                )
                .unwrap_or_else(|error| panic!("Cloud inference configuration: {error}")),
            )
        });
    if let Some(root) = workspace_root.as_deref() {
        let migrated = awaken_server::migrate_legacy_skill_registry(root, skill_store.as_ref())
            .await
            .unwrap_or_else(|error| panic!("legacy Skill migration failed: {error}"));
        if migrated > 0 {
            eprintln!("migrated {migrated} legacy Skill aggregate(s)");
        }
    }
    // Finish or compensate any credential mutation interrupted by a prior hard
    // process crash before exposing the management/data planes.
    if let Err(error) = awaken_credential_vault::repo::recover_credential_mutations(
        secrets.as_ref(),
        credentials.as_ref(),
    )
    .await
    {
        eprintln!("credential mutation recovery failed: {error}");
    }
    match awaken_credential_vault::repo::reconcile_credential_inventory(
        secrets.as_ref(),
        credentials.as_ref(),
    )
    .await
    {
        Ok(report) if !report.missing_material.is_empty() => eprintln!(
            "credential inventory is missing referenced material: {:?}",
            report.missing_material
        ),
        Err(error) => eprintln!("credential inventory reconciliation failed: {error}"),
        _ => {}
    }
    spawn_credential_mutation_reconciliation(secrets.clone(), credentials.clone());
    // ONE resource-binding store shared by the admin router (which authors an agent's
    // resources) and the config service (which reads them into resource prompts +
    // mounts at compile) — so a binding authored through the API reaches the compiled
    // config (ADR-0038).
    // The Managed vault state, shared by the vault router (authoring) and the managed
    // state (data plane): a credential entered through either surface is the same row.
    let vault_state = Arc::new(
        VaultState::new(secrets.clone(), credentials.clone())
            // The live MCP probe is backed by ext-mcp here — the only place the MCP
            // client is named for validation (mirrors the GenaiProbe pattern).
            .with_probe(Arc::new(ExtMcpProbe)),
    );
    // Environments + work queue, shared with the session state so `POST /v1/sessions`
    // resolves an environment's networking policy (egress on/off) at creation.
    let env_state = environments;

    let credential_materializer = awaken_runtime_host::PinnedCredentialMaterializer::new(
        credentials.clone(),
        secrets.clone(),
    );
    let web_search_providers = assembly
        .web_search_providers
        .unwrap_or_else(awaken_runtime_host::WebSearchProviderRegistry::builtins);
    let executor_model_capabilities =
        Arc::new(awaken_server::model_directory::installed_executor_model_capabilities());
    let model_wiring = match model_composition {
        PublicationModelComposition::PublishedProviders => PublicationModelWiring {
            executor: Arc::new(awaken_runtime_host::NoModelConfiguredExecutor),
            model_ref: awaken_runtime_host::UNCONFIGURED_MODEL_REF.to_string(),
            publication_resolver: Arc::new(
                awaken_server::model_resolver::CatalogModelPublicationResolver::from_repo(
                    catalog.clone(),
                    credentials.clone(),
                )
                .with_executor_capabilities(executor_model_capabilities.clone())
                .with_profiles(profiles.clone())
                .with_worker_directory(awaken_server::worker_directory())
                .with_brokered_access(cloud_models_enabled),
            ),
            materializer: Some(Arc::new({
                let materializer = awaken_server::inference_materializer::CredentialInferenceMaterializer::from_pinned(
                    credential_materializer.clone(),
                )
                .with_brokered_mode(cloud_models_enabled);
                match &brokered_client {
                    Some(client) => materializer.with_brokered_client(client.clone()),
                    None => materializer,
                }
            })),
        },
        PublicationModelComposition::HostedPublication { resolver } => PublicationModelWiring {
            executor: Arc::new(awaken_runtime_host::NoModelConfiguredExecutor),
            model_ref: awaken_runtime_host::UNCONFIGURED_MODEL_REF.to_string(),
            publication_resolver: resolver,
            materializer: None,
        },
        PublicationModelComposition::Host { executor, binding } => PublicationModelWiring {
            executor,
            model_ref: binding.model_ref.clone(),
            publication_resolver: Arc::new(ExactHostModelPublicationResolver { binding }),
            materializer: None,
        },
    };
    let global = awaken_runtime_host::authorable_tools();
    let tool_catalog: Arc<dyn ToolCatalogSource> = Arc::new(ScopedToolCatalog::new(
        global.clone(),
        RESERVED_ADMIN_SCOPE,
        awaken_admin_assistant::admin_tool_descriptors(),
    ));
    // Resolve `Auto` against the LIVE catalog repo (not the frozen seed), so a
    // model an operator adds AFTER startup is visible when we re-publish the
    // reserved-scope assistant. The resolver is mandatory: no config service can
    // be constructed with an implicit model fallback.
    let web_search_publication_resolver =
        assembly.web_search_publication_resolver.unwrap_or_else(|| {
            Arc::new(awaken_runtime_host::WebSearchPublicationResolver::new(
                web_search_providers.clone(),
            ))
        });
    // One Coordinator-owned executable projection serves every execution read.
    // Composition selects only the local, HTTP, or durable boundary adapter.
    let executable_agent_catalog = executable_agent_wiring.catalog;
    let executable_agent_registrar = executable_agent_wiring.registrar;
    let executable_agent_private_router = executable_agent_wiring.private_router;
    let config_service = Arc::new(
        ConfigService::new(
            model_wiring.publication_resolver,
            executable_agent_registrar,
        )
        .with_credential_reference_validator(Arc::new(
            awaken_control::CredentialRevisionValidator::new(credentials.clone()),
        ))
        .with_plugin_publication_resolver(web_search_publication_resolver)
        .with_resources(resource_store.clone()),
    );
    // Re-submit durable Control publications through the same registration port
    // used by live publication. The catalog is a rebuildable Coordinator projection,
    // never a second publication source of truth.
    let warmed = if role == config::Role::Coordinator {
        0
    } else {
        config_service
            .reconcile_registrations(
                config.as_ref(),
                &awaken_tenancy::ScopeId::from(platform_workspace.as_str()),
            )
            .await
            .unwrap_or_else(|error| {
                panic!("executable Agent registration recovery failed: {error}")
            })
    };
    if warmed > 0 {
        eprintln!("config: reconciled {warmed} durable Agent publication(s)");
    }
    let plane = ConfigPlane::new(config_service.clone(), config, tool_catalog);
    // Seed the in-console Admin Assistant as an ordinary published agent in the
    // reserved scope (ADR-0052 D1/D2). Best-effort: a server booted without a
    // resolvable model still starts (the assistant stays a draft until one is set).
    let assistant_catalog = catalog.snapshot().await.unwrap_or_default();
    let assistant_credentials = credentials
        .list(&platform_workspace)
        .await
        .unwrap_or_default();
    let assistant_selection = assistant_selection::select(
        &assistant_catalog,
        &assistant_credentials,
        &assembly.local_acp_observations,
    );
    if role != config::Role::Coordinator
        && let Some(assistant_selection) = assistant_selection
        && let Err(err) =
            awaken_control::seed_admin_assistant(&plane, &platform_workspace, assistant_selection)
                .await
    {
        eprintln!("admin assistant not seeded (configure a model, then republish): {err}");
    }
    // When an operator adds a model AFTER startup, re-publish the reserved-scope
    // assistant so its `Auto` binding resolves off `unconfigured` onto the new model.
    // Same reserved scope + agent id `seed_admin_assistant` published under, so the
    // reconcile targets exactly the seeded agent (ADR-0052 D2). Fired by the middleware
    // layer below on a successful catalog write. `ConfigPlane` is `Clone`.
    let reconciler = Arc::new(awaken_runtime_host::ConfigServiceReconciler::new(
        plane.clone(),
        RESERVED_ADMIN_SCOPE,
        platform_workspace.clone(),
        vec![awaken_admin_assistant::ADMIN_ASSISTANT_AGENT_ID.to_string()],
    ));
    // The LIVE data-plane resource inventory (ADR-0038): memory-store ids from the durable
    // Resource Catalog (the same aggregate Session resolution reads, so this stays
    // consistent) + skill ids from the shared skill store. Unlike before, this is now
    // reachable at wire time because both handles are assembled by the composition root.
    let resource_inventory = Arc::new(awaken_control::HostResourceInventory::new(
        resource_catalog.clone(),
        skill_store.clone(),
        platform_workspace.clone(),
    ));
    // The management tool executables (ADR-0052 D3/D4): the capability reader reads the
    // shared catalog + advertised tools; the validator runs the publish-time compile
    // check on drafts in the tenant scope; Runtime history records every call and
    // mutating tools additionally enter the durable config-change path.
    let capability_reader = Arc::new(awaken_control::CatalogCapabilityReader::new(
        // The LIVE catalog repo — models/providers an operator adds after startup
        // are visible on the next capabilities call (not a frozen seed snapshot).
        catalog.clone(),
        &global,
        // The installable plugins (state_machine / memory / compact) so the assistant
        // knows it CAN author a state machine etc. — not an empty list (it would
        // otherwise refuse, thinking no plugins exist).
        &awaken_runtime_host::authorable_config_sections_with_web_search(&web_search_providers),
        // The config plane, to list existing agent ids in the tenant scope.
        plane.clone(),
        platform_workspace.clone(),
        // LIVE data-plane inventory: memory-store ids (durable registry) + skill ids
        // (shared skill store). Both handles are assembled by this composition root
        // before the host, so the assistant enumerates real memory stores + skills.
        Some(resource_inventory),
    ));
    let admin_execs = awaken_admin_assistant::admin_tools(
        capability_reader,
        Arc::new(awaken_control::ConfigServiceDraftValidator::new(
            plane.clone(),
            platform_workspace.clone(),
        )),
        // Persist/read drafts as unpublished config agents through the same plane the
        // editor's Save uses, in the tenant/default scope (ADR-0052).
        Arc::new(awaken_control::ConfigServiceDraftStore::new(
            plane.clone(),
            platform_workspace.clone(),
            resource_store.clone(),
        )),
        // Author environments through the SAME managed-plane registry the console's
        // New-environment modal drives, so `admin_draft_environment` persists for real.
        Arc::new(awaken_control::EnvironmentStateAuthor::new(
            env_state.clone(),
        )),
        Arc::new(awaken_admin_assistant::TracingAuditSink),
    );
    // The MCP server export is explicit and capability-token gated. Clone the
    // management executables before the host takes ownership, pairing each with
    // its authoritative descriptor rather than reconstructing a schema here.
    let mcp_export = awaken_server::mcp_export::router(
        awaken_admin_assistant::admin_tool_descriptors(),
        admin_execs.clone(),
        mcp_bearer_token,
    );

    // The authoring / authz plane (admin + vault + webhooks + user profiles +
    // deployments + environments + config plane + capabilities), guard applied over
    // admin + vault only. Returns the webhook sink the data plane feeds.
    // Deployment and DeploymentRun are Control-owned durable truth. Restore the
    // existing aggregate from the same repository that owns Managed Sessions;
    // `DeploymentState::new()` is reserved for explicitly ephemeral tests.
    let deployment_state = Arc::new(
        awaken_protocol_managed::DeploymentState::with_repository(deployments)
            .await
            .unwrap_or_else(|error| panic!("restore Deployment state: {error}")),
    );
    deployment_state.bind_rate_limiter(managed_rate_limiter.clone());
    if role == config::Role::Control
        && let Some(config) = deployment_session_launch.as_ref()
    {
        deployment_session_launch::bind_control(deployment_state.clone(), config)
            .unwrap_or_else(|error| panic!("Deployment Session launch configuration: {error}"));
    }
    // Keep the IAM handles for the sibling resource PEP. The authoring router owns
    // its PEP; File/Memory/Skill routes are wrapped independently after the data
    // router is assembled, so neither plane depends on the other's services.
    let resource_iam = iam.clone();
    let resource_remote_iam = remote_iam.clone();
    let application_access = Arc::new(awaken_authz_enforce::ApplicationAccessStore::new());
    let live_runtime_capabilities = Arc::new(LiveRuntimeCapabilities {
        initial: assembly.local_acp_observations.clone(),
        workers: awaken_server::worker_directory(),
        credentials: credentials.clone(),
        workspace: platform_workspace.clone(),
    });
    let (control, webhook_sink) =
        awaken_control::control_router(awaken_control::ControlRouterInput {
            platform_workspace: platform_workspace.clone(),
            catalog: catalog.clone(),
            credentials: credentials.clone(),
            secrets: secrets.clone(),
            profiles,
            webhook_store,
            sessions: sessions.clone(),
            resource_store: resource_store.clone(),
            probe: Arc::new(credential_probe::GenaiProbe),
            model_discovery: Arc::new(awaken_server::model_discovery::GenaiModelDiscovery::new(
                secrets.clone(),
            )),
            brokered_catalog: brokered_client
                .clone()
                .map(|client| client as Arc<dyn awaken_admin_config_api::BrokeredCatalogDiscovery>),
            cloud_models_enabled,
            vault_state: vault_state.clone(),
            env_state: env_state.clone(),
            deployment_state: deployment_state.clone(),
            plane,
            global_tools: global,
            plugins: awaken_runtime_host::platform_plugin_capabilities_with_web_search(
                &web_search_providers,
            ),
            runtimes: live_runtime_capabilities.clone(),
            org_id: Some(org_id.clone()),
            iam,
            remote_iam,
            local_browser_auth,
            application_access: application_access.clone(),
        });

    if role == config::Role::Control {
        return process_surface::finish(
            control,
            mcp_export,
            Some(reconciler),
            platform_workspace,
            managed_rate_limiter,
        );
    }

    // The data plane: the host runs the server model, resolves a session's agent to
    // its installed config, and carries the management tool executables so the
    // reserved-scope assistant can call them. It shares the SAME skill store and
    // Resource Catalog the capability inventory reads, so a skill or memory store the
    // host serves is exactly what the assistant enumerates, and identity survives a
    // restart.
    let mut host_builder = match deployment {
        Some(deployment) => SharedHost::new_with_resource_plane_and_deployment(
            model_wiring.executor,
            model_wiring.model_ref,
            resource_plane,
            deployment,
        ),
        None => SharedHost::new_with_resource_plane(
            model_wiring.executor,
            model_wiring.model_ref,
            resource_plane,
        ),
    };
    host_builder = host_builder
        .with_local_workspace(platform_workspace.clone())
        .with_credential_materializer(credential_materializer.clone())
        .with_web_search_provider_registry(web_search_providers)
        .with_acp_tool_exporter(Arc::new(awaken_server::mcp_export::SessionToolExporter))
        .with_remote_attempt_executor(awaken_server::a2a_attempt_executor(Some(
            credential_materializer.clone(),
        )))
        .with_agent_publications(executable_agent_catalog.clone())
        .with_agent_resource_references(executable_agent_catalog.clone())
        .with_admin_tools(admin_execs);
    host_builder = host_builder.with_tool_executor_provider(Arc::new(
        awaken_server::placement::ConfigToolExecutorProvider::from_declared_hands(
            Arc::new(executable_agent_registration::CatalogDeclaredHandSource(
                executable_agent_catalog.clone(),
            )),
            hand_executors,
        ),
    ));
    if let Some(materializer) = model_wiring.materializer {
        host_builder = host_builder.with_inference_materializer(materializer);
    }
    host_builder.install_memory_extraction_repository(memory_extractions);
    awaken_server::install_platform_memory_data_plane(&host_builder);
    // Production ACP wiring (`acp:*` threads): the environment advertises only the
    // installed CLI/sandbox capability. Provider coordinates and credentials are
    // realized from the same publication-pinned DB facts as native inference.
    let hand_factory = awaken_server::relay_hand_executor_factory();
    let host_builder = host_builder
        .with_session_environment_from_deployment(Some(hand_factory))
        .await
        .with_acp_from_deployment(Some(credential_materializer.clone()))
        .await;
    // Last-mile backend wiring the standard process does not assemble itself, injected
    // by the composition root (a scenario that serves external-CLI sessions).
    let host_builder = match customize_host {
        Some(customize) => customize(host_builder),
        None => host_builder,
    };
    let host = Arc::new(host_builder);
    let resource_reclamation = Arc::new(awaken_runtime_host::HostResourceReclamation::new(
        host.clone(),
        resource_catalog.clone(),
    ));
    let resource_reclaimer = Arc::new(
        awaken_resource_reclaimer::ResourceReclaimer::new(
            format!("awaken-resource-reclaimer:{}", std::process::id()),
            30_000,
            host.resource_lifecycle()
                .expect("resource-plane composition installs lifecycle repository"),
            resource_reclamation.clone(),
        )
        .expect("construct resource reclaimer")
        .with_guard(resource_reclamation),
    );
    let now_ms = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or_default()
    };
    if let Ok(summary) = resource_reclaimer.reconcile(now_ms(), 256).await
        && summary.completed > 0
    {
        eprintln!("reclaimed {} durable resource(s)", summary.completed);
    }
    let recurring_resource_reclaimer = resource_reclaimer.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or_default();
            if let Err(error) = recurring_resource_reclaimer.reconcile(now, 256).await {
                eprintln!("resource reclamation retry remains pending: {error}");
            }
        }
    });
    let managed_state = Arc::new(
        ManagedState::new_with_mcp(
            ManagedHost::new(host.clone())
                .with_resource_validator(resource_catalog.clone())
                .with_credential_materializer(credential_materializer),
        )
        .with_vaults(vault_state)
        .with_environments(env_state)
        .with_resource_catalog(resource_catalog.clone())
        .with_resource_purge_scheduler(host.clone())
        // Share the SAME config plane `/v1/agents` reads, so a session inheriting a
        // published agent's model sees the authoritative config-plane truth (M2).
        .with_config_source(executable_agent_catalog)
        .with_session_repo(sessions)
        .with_lifecycle_sink(webhook_sink),
    );
    let reconciled_resource_activations = managed_state.reconcile_resource_activations().await;
    if reconciled_resource_activations > 0 {
        eprintln!(
            "reconciled {reconciled_resource_activations} durable Session resource activation(s)"
        );
    }
    let reconciled_mcp_attachments = managed_state.reconcile_mcp_attachments().await;
    if reconciled_mcp_attachments > 0 {
        eprintln!("reconciled {reconciled_mcp_attachments} durable Session MCP projection(s)");
    }
    let _ = managed_state.spawn_realization_lease_supervisor();
    let deployment_session_private_router = deployment_session_launch::bind_runtime(
        role,
        deployment_state.clone(),
        managed_state.clone(),
        deployment_session_launch.as_ref(),
    )
    .unwrap_or_else(|error| panic!("Deployment Session launch configuration: {error}"));
    // Workspace path addressing (ADR-0048 D3 / ADR-0051): wrap the fully-merged flat
    // surface so a `/v1/workspaces/{ws}/…` request is captured, rewritten to its flat
    // `/v1/…` form, and its `{ws}` stamped as the edge scope before it re-enters
    // routing. Flat requests fall through unchanged. The same assembly returns the
    // DreamState it mounted, so scheduling cannot target a parallel instance.
    let model_directory = Arc::new(
        awaken_server::model_directory::CatalogModelDirectory::with_source(
            catalog.clone(),
            credentials.clone(),
            live_runtime_capabilities,
        ),
    );
    let (mut data, dream_state) =
        awaken_server::mount_with_managed_application_access_models_and_dreams(
            host,
            managed_state,
            resource_catalog,
            application_access,
            model_directory,
            dream_repository,
            worker_authenticator,
        );
    data = data.merge(executable_agent_private_router);
    data = data.merge(deployment_session_private_router);
    // One timer drives every Managed periodic trigger. Deployment remains the cron
    // authority; Dream policies submit the same durable DreamJob as manual create.
    let scheduled_deployments = deployment_state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
        loop {
            interval.tick().await;
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or_default();
            if let Err(error) = scheduled_deployments.tick_and_launch(now_ms).await {
                eprintln!("scheduled Deployment tick failed: {error}");
            }
            if let Err(error) = dream_state.tick_policies(now_ms).await {
                eprintln!("scheduled Dream policy tick failed: {error}");
            }
        }
    });
    if let Some(iam) = resource_iam {
        data = data.layer(axum::middleware::from_fn_with_state(
            iam,
            awaken_control::authz::resource_guard,
        ));
    } else if let Some(remote_iam) = resource_remote_iam {
        data = data.layer(axum::middleware::from_fn_with_state(
            remote_iam,
            awaken_control::authz::cloud_resource_guard,
        ));
    }
    let (flat, mcp_export) = match role {
        config::Role::AllInOne => (data.merge(control), mcp_export),
        config::Role::Coordinator => (data, Router::new()),
        config::Role::Control => unreachable!("Control returned before Coordinator assembly"),
        config::Role::Worker => unreachable!("Worker has its own process composition"),
    };
    process_surface::finish(
        flat,
        mcp_export,
        (role == config::Role::AllInOne)
            .then_some(reconciler as Arc<dyn awaken_runtime_host::PublicationBindingReconciler>),
        platform_workspace,
        managed_rate_limiter,
    )
}

/// Resolve the hidden local Org from one composition-root seam. Self-managed
/// deployments may explicitly configure it; single-machine mode never asks the
/// user and consistently uses the default Org.
fn local_org_id() -> String {
    awaken_control::DEFAULT_ORG_ID.to_owned()
}

// The seal-key resolution tests moved to `awaken_credential_vault::sealed`, the
// single home of `resolve_seal_key_hex` / `parse_seal_key`.

#[cfg(test)]
mod runtime_session_store_tests {
    use super::*;
    use awaken_protocol_managed::{
        ApplicationContributionState, ControlSessionCreationInputs, EnvironmentFingerprint,
        EnvironmentRevision, EnvironmentSnapshot, IdempotencyRecord, PersistedSession,
        SessionBaselineState, SessionCreationIntent, SessionNetworkPolicy, stable_fingerprint,
    };
    use awaken_runtime_host::ModelPublicationResolver;

    fn creation_intent() -> SessionCreationIntent {
        SessionCreationIntent {
            control: ControlSessionCreationInputs {
                environment: EnvironmentSnapshot {
                    environment_id: "env_local".into(),
                    revision: EnvironmentRevision(1),
                    config_fingerprint: EnvironmentFingerprint("env-local".into()),
                    sandbox: serde_json::json!({}),
                    sandbox_provisioning: Default::default(),
                    packages: Default::default(),
                    network: SessionNetworkPolicy::Unrestricted,
                    credential_realization: awaken_runtime_contract::CredentialRealizationProfile {
                        inference_holder: awaken_runtime_contract::PlaintextHolder::new(
                            awaken_runtime_contract::PlaintextBoundary::Workload,
                            "awaken.workload.acp",
                        ),
                        mcp_holder: awaken_runtime_contract::PlaintextHolder::new(
                            awaken_runtime_contract::PlaintextBoundary::Worker,
                            "awaken.worker",
                        ),
                        resource_holder: awaken_runtime_contract::PlaintextHolder::new(
                            awaken_runtime_contract::PlaintextBoundary::Worker,
                            "awaken.worker",
                        ),
                    },
                },
                agent_id: "assistant".into(),
                model: "test-model".into(),
                execution_model_ref: "test-model".into(),
                runtime: None,
                mcp_authoring: Default::default(),
                delegate_ids: Vec::new(),
                toolsets: Vec::new(),
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: Vec::new(),
                resources: Default::default(),
                initial_mcp: Vec::new(),
            },
            application: ApplicationContributionState::Absent,
        }
    }

    fn session(id: &str) -> PersistedSession {
        PersistedSession {
            session_id: id.to_string(),
            revision: Default::default(),
            baseline: SessionBaselineState::Preparing(creation_intent()),
            title: None,
            metadata: Default::default(),
            agent_tools: Vec::new(),
            environment_binding: None,
            mcp: Default::default(),
            resources: Default::default(),
            realization: None,
            status: "idle".to_string(),
            archived_at: None,
        }
    }

    #[tokio::test]
    async fn runtime_storage_reopens_the_session_and_its_owner_fence() {
        let dir = tempfile::tempdir().expect("temporary runtime storage");
        {
            let stores = deployment_stores_for_runtime_storage(Some(dir.path()));
            let value = session("sesn-restart");
            let payload_hash = stable_fingerprint(&value);
            stores
                .sessions
                .create(
                    "workspace-a",
                    value,
                    IdempotencyRecord {
                        key: "test:runtime-session-restart".into(),
                        payload_hash,
                    },
                    Vec::new(),
                )
                .await
                .unwrap();
        }

        let reopened = deployment_stores_for_runtime_storage(Some(dir.path()));
        assert_eq!(
            reopened.sessions.owner("sesn-restart").await.as_deref(),
            Some("workspace-a")
        );
        assert_eq!(
            reopened
                .sessions
                .get("sesn-restart")
                .await
                .expect("session survives restart")
                .baseline,
            SessionBaselineState::Preparing(creation_intent())
        );
    }

    #[test]
    fn session_and_deployment_ports_share_one_physical_repository() {
        // Cause/effect decision table: D1 durable runtime storage -> Session and
        // Deployment typed ports point at one concrete repository allocation;
        // D2 ephemeral composition -> the same single-allocation invariant holds.
        // A different address would expose a parallel Deployment truth before
        // `DeploymentState::with_repository` is assembled.
        let dir = tempfile::tempdir().expect("temporary runtime storage");
        let durable = deployment_stores_for_runtime_storage(Some(dir.path()));
        assert_eq!(
            Arc::as_ptr(&durable.sessions) as *const (),
            Arc::as_ptr(&durable.deployments) as *const (),
            "D1"
        );

        let ephemeral = deployment_stores_for_runtime_storage(None);
        assert_eq!(
            Arc::as_ptr(&ephemeral.sessions) as *const (),
            Arc::as_ptr(&ephemeral.deployments) as *const (),
            "D2"
        );
    }

    #[tokio::test]
    async fn scenario_host_resolves_model_only_authoring_to_its_exact_binding() {
        let binding =
            awaken_runtime_contract::resolved::ModelBinding::new("default", "scenario", "default");
        let resolver = ExactHostModelPublicationResolver {
            binding: binding.clone(),
        };

        let resolved = resolver
            .resolve_models(
                &awaken_tenancy::ScopeId::from("workspace-a"),
                &awaken_config_store::ModelSelection::Pinned(
                    awaken_runtime_contract::resolved::ModelBinding::new("", "scenario", ""),
                ),
                &[],
            )
            .await
            .expect("model-only SDK/UI selection resolves through the host adapter");

        assert_eq!(resolved.primary.binding, binding);
    }

    #[tokio::test]
    async fn scenario_host_rejects_a_conflicting_provider_qualified_binding() {
        let resolver = ExactHostModelPublicationResolver {
            binding: awaken_runtime_contract::resolved::ModelBinding::new(
                "default", "scenario", "default",
            ),
        };

        let error = resolver
            .resolve_models(
                &awaken_tenancy::ScopeId::from("workspace-a"),
                &awaken_config_store::ModelSelection::Pinned(
                    awaken_runtime_contract::resolved::ModelBinding::new(
                        "other", "scenario", "default",
                    ),
                ),
                &[],
            )
            .await
            .expect_err("an exact provider selection cannot drift to the host executor");

        assert!(error.to_string().contains("cannot publish model"));
    }
}

#[cfg(test)]
mod process_role_surface_tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt as _;

    use super::*;

    /// Cause/effect decision table:
    ///
    /// | role | Control API | Coordinator API | registration API | launch API |
    /// | --- | --- | --- | --- | --- |
    /// | Control | mounted | absent | absent | absent |
    /// | Coordinator | absent | mounted | authenticated | independently authenticated |
    ///
    /// AllInOne merging is covered by the existing full-surface integration
    /// suites; this test owns the two exclusion rules that those suites cannot
    /// prove.
    #[tokio::test]
    async fn service_roles_expose_only_their_owned_api() {
        let app = assemble_process_router(
            in_memory_deployment_stores(),
            None,
            None,
            None,
            PublicationModelComposition::PublishedProviders,
            ProcessAssemblyOptions {
                role: config::Role::Control,
                ..Default::default()
            },
            None,
        )
        .await;

        let control = app
            .clone()
            .oneshot(
                Request::get("/v1/config/catalog")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(control.status(), StatusCode::OK);

        let registration = app
            .clone()
            .oneshot(
                Request::post(awaken_executable_agent_catalog::EXECUTABLE_AGENT_REGISTER_PATH)
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer registration-token")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(registration.status(), StatusCode::NOT_FOUND);

        let launch = app
            .clone()
            .oneshot(
                Request::post(awaken_protocol_managed::DEPLOYMENT_SESSION_LAUNCH_PATH)
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer deployment-launch-token")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(launch.status(), StatusCode::NOT_FOUND);

        let session = app
            .oneshot(Request::get("/v1/sessions").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(session.status(), StatusCode::NOT_FOUND);

        let token_dir = tempfile::tempdir().unwrap();
        let launch_token_file = token_dir.path().join("deployment-launch-token");
        std::fs::write(&launch_token_file, "deployment-launch-token\n").unwrap();
        let launch_config = config::DeploymentSessionLaunchConfig::resolve(
            config::Role::Coordinator,
            None,
            Some(launch_token_file),
        )
        .unwrap();
        let app = assemble_process_router(
            in_memory_deployment_stores(),
            None,
            None,
            None,
            PublicationModelComposition::PublishedProviders,
            ProcessAssemblyOptions {
                role: config::Role::Coordinator,
                executable_agent_wiring: Some(
                    executable_agent_registration::ExecutableAgentWiring::local_server(
                        "registration-token",
                    ),
                ),
                deployment_session_launch: Some(launch_config),
                ..Default::default()
            },
            None,
        )
        .await;
        let control = app
            .clone()
            .oneshot(
                Request::get("/v1/config/catalog")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(control.status(), StatusCode::NOT_FOUND);

        let session = app
            .clone()
            .oneshot(Request::get("/v1/sessions").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(session.status(), StatusCode::OK);
        let registration = app
            .clone()
            .oneshot(
                Request::post(awaken_executable_agent_catalog::EXECUTABLE_AGENT_REGISTER_PATH)
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer registration-token")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(registration.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let launch = app
            .oneshot(
                Request::post(awaken_protocol_managed::DEPLOYMENT_SESSION_LAUNCH_PATH)
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer wrong-token")
                    .body(Body::from(
                        serde_json::json!({
                            "deployment_id": "depl_role",
                            "deployment_run_id": "drun_role",
                            "workspace_id": "workspace-role",
                            "agent": {"id": "agent-role", "type": "agent", "version": 1},
                            "environment_id": "env_local",
                            "metadata": {},
                            "initial_events": [],
                            "resources": [],
                            "vault_ids": []
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(launch.status(), StatusCode::UNAUTHORIZED);
    }

    #[derive(Debug)]
    struct RecordingHostedResolver {
        called: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl awaken_runtime_host::ModelPublicationResolver for RecordingHostedResolver {
        async fn resolve_models(
            &self,
            _workspace: &awaken_tenancy::ScopeId,
            _selection: &awaken_config_store::ModelSelection,
            _candidates: &[awaken_runtime_contract::resolved::ModelBinding],
        ) -> Result<
            awaken_runtime_host::ResolvedPublicationModels,
            awaken_runtime_host::PublicationResolutionError,
        > {
            self.called.store(true, Ordering::SeqCst);
            Err(awaken_runtime_host::PublicationResolutionError::MissingPrimary)
        }
    }

    /// Causal graph:
    /// hosted resolver injection -> canonical ConfigService publication
    /// -> the injected resolver is the only model-selection authority
    /// -> its fail-closed result is returned without consulting the local catalog.
    ///
    /// Decision table:
    /// | control composition | resolver result | local catalog | outcome |
    /// | --- | --- | --- | --- |
    /// | default open | any | authoritative | catalog resolver decides |
    /// | hosted injection | success | irrelevant | injected candidate publishes |
    /// | hosted injection | failure | populated/empty | publication fails closed |
    #[tokio::test]
    async fn hosted_control_uses_the_injected_publication_resolver() {
        let called = Arc::new(AtomicBool::new(false));
        let app = assemble_process_router(
            in_memory_deployment_stores(),
            None,
            None,
            None,
            PublicationModelComposition::HostedPublication {
                resolver: Arc::new(RecordingHostedResolver {
                    called: called.clone(),
                }),
            },
            ProcessAssemblyOptions {
                role: config::Role::Control,
                ..Default::default()
            },
            None,
        )
        .await;

        let authored = app
            .clone()
            .oneshot(
                Request::put("/v1/config/agents/hosted-agent")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "Hosted",
                            "model": {
                                "provider_identity_ref": "provider-account-a",
                                "model_ref": "model-a",
                                "backend_ref": "hosted"
                            }
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authored.status(), StatusCode::OK);

        let published = app
            .oneshot(
                Request::post("/v1/config/agents/hosted-agent/publish")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(published.status(), StatusCode::CONFLICT);
        assert!(called.load(Ordering::SeqCst));
    }
}
