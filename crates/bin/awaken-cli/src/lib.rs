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
mod control_component;
mod credential_probe;
mod exact_host_model;
mod executable_agent_registration;
mod identity;
mod observation_reconcile;
mod process_assembly_options;
mod process_stores;
mod process_surface;
mod resource_component;
mod worker_transport_security;

use std::sync::Arc;

use awaken_config_service::ManagementAuditPlane;
use awaken_protocol_managed::{EnvironmentState, ManagedState};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_host::{ExtMcpProbe, ManagedHost, SharedHost};
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
use control_component::{assemble_control_process_router, control_component_for_process};
use identity::identity_wiring;
use process_assembly_options::{ProcessAssemblyOptions, local_model_supply};
#[cfg(test)]
use process_stores::role_composes_resource_component;
use process_stores::{
    ControlStores, CoordinatorStores, MigrationComponent, PostgresSchemaMode, ProcessStores,
    migration_manifest, role_owns_control_component, role_owns_managed_execution,
};
use resource_component::{ephemeral_resource_component, open_resource_component};
pub use worker_transport_security::load_request_authorizer as load_worker_request_authorizer;
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
        resolver: Arc<dyn awaken_config_service::ModelPublicationResolver>,
    },
    Host {
        executor: Arc<dyn LlmExecutor>,
        binding: awaken_runtime_contract::resolved::ModelBinding,
    },
}

/// Concrete wiring produced by one legal composition mode. Keeping these four
/// values together prevents a provider resolver from being paired with a host
/// executor or a Host publication from receiving a credential materializer.
struct RuntimeModelWiring {
    executor: Arc<dyn LlmExecutor>,
    model_ref: String,
    materializer: Option<Arc<dyn awaken_runtime_host::InferenceExecutorMaterializer>>,
}

/// Publication policy plus a deferred runtime choice. Standalone Control uses
/// only the resolver; execution adapters are realized only by runtime assembly.
struct PublicationModelAssembly {
    publication_resolver: Arc<dyn awaken_config_service::ModelPublicationResolver>,
    runtime: RuntimeModelAssembly,
}

#[derive(Clone)]
enum RuntimeModelAssembly {
    PublishedProviders,
    NoModelConfigured,
    Host {
        executor: Arc<dyn LlmExecutor>,
        model_ref: String,
    },
}
#[derive(Clone)]
struct ControlServicePorts {
    audit: Arc<dyn awaken_config_service::ManagementAuditRepository>,
    credentials: Arc<dyn awaken_protocol_managed::SessionCredentialSource>,
    webhooks: Arc<dyn awaken_webhook_managed::LifecycleFactDelivery>,
}

impl ControlServicePorts {
    fn remote(
        client: Arc<awaken_server::control_service_boundary::HttpControlServiceClient>,
    ) -> Self {
        Self {
            audit: client.clone(),
            credentials: client.clone(),
            webhooks: client,
        }
    }
}
/// Router plus the cleartext local setup handoff printed by the CLI once.
pub struct ProcessAssembly {
    pub router: Router,
    pub local_setup: Option<awaken_control::LocalSetupHandoff>,
}

/// Ephemeral deployment stores: everything in process memory (dev / e2e default).
fn in_memory_process_stores() -> ProcessStores {
    let sessions = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("open ephemeral managed Session repository"),
    );
    let admin = Arc::new(
        awaken_admin_config_api::SqliteAdminStore::open_in_memory()
            .expect("open ephemeral admin store"),
    );
    ProcessStores {
        workspace_root: None,
        control: Some(ControlStores {
            catalog: Arc::new(awaken_model_catalog::repo::InMemoryCatalogRepo::new()),
            credentials: Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
            secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
            profiles: admin.clone(),
            resources: admin.clone(),
            webhooks: admin,
            config: Arc::new(
                awaken_config_store::SqliteConfigStore::open_in_memory()
                    .expect("open config store"),
            ),
        }),
        coordinator: Some(CoordinatorStores {
            resource_component: ephemeral_resource_component(),
            sessions: sessions.clone(),
            deployments: sessions.clone(),
            memory_extractions: sessions.clone(),
            dream_repository: sessions,
        }),
        environments: Arc::new(EnvironmentState::new()),
    }
}

#[cfg(test)]
fn in_memory_control_stores() -> ProcessStores {
    let mut stores = in_memory_process_stores();
    stores.coordinator = None;
    stores
}

#[cfg(test)]
fn in_memory_split_coordinator() -> (ProcessStores, ControlServicePorts) {
    let mut stores = in_memory_process_stores();
    let control = stores
        .control
        .take()
        .expect("fixture starts with Control stores");
    let audit = Arc::new(ManagementAuditPlane::new(control.config.clone()));
    let credentials = Arc::new(
        awaken_protocol_managed::VaultState::new(
            control.secrets.clone(),
            control.credentials.clone(),
        )
        .with_probe(Arc::new(ExtMcpProbe)),
    );
    let webhooks = awaken_webhook_managed::config_plane_lifecycle_delivery(
        control.webhooks,
        control.secrets,
        None,
    );
    (
        stores,
        ControlServicePorts {
            audit,
            credentials,
            webhooks,
        },
    )
}

/// Keep the Managed Session aggregate durable whenever the runtime itself is
/// durable, even when the rest of the process composition intentionally remains
/// ephemeral. A restarted runtime can only rehydrate a governed Session when its
/// configuration and owner fence survive beside the committed thread facts.
#[cfg(test)]
fn process_stores_for_runtime_storage(storage_dir: Option<&std::path::Path>) -> ProcessStores {
    let mut stores = in_memory_process_stores();
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
    let coordinator = stores
        .coordinator
        .as_mut()
        .expect("test composition owns Coordinator");
    coordinator.sessions = sessions.clone();
    coordinator.deployments = sessions.clone();
    coordinator.memory_extractions = sessions.clone();
    coordinator.dream_repository = sessions;
    stores
}

/// Open the canonical role-owned stores selected by
/// [`ControlStoreConfig`](awaken_control::ControlStoreConfig). The compatibility
/// bundle resolves backend addresses, but this function acquires only the group
/// owned by `role`; split Control and Coordinator never share authority stores.
async fn open_process_stores(
    cfg: awaken_control::ControlStoreConfig,
    resource_component: Option<awaken_resource_contract::ResourceComponent>,
    workspace_root: std::path::PathBuf,
    key: Option<&[u8; 32]>,
    role: config::Role,
    postgres_schema: PostgresSchemaMode,
    open_environment_stores: bool,
) -> Result<ProcessStores, String> {
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

    let opens_control = role_owns_control_component(role);
    let catalog: Option<Arc<dyn awaken_model_catalog::repo::CatalogRepo>> = if opens_control {
        ensure_parent(&cfg.catalog)?;
        Some(match &cfg.catalog {
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
        })
    } else {
        None
    };

    // The credential repo and its sealed-secret blobs share the one credential backend.
    let (credentials, secrets) = if opens_control {
        ensure_parent(&cfg.credential)?;
        let key = key.ok_or_else(|| "Control stores require the Control seal key".to_owned())?;
        let pair: (
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
        (Some(pair.0), Some(pair.1))
    } else {
        (None, None)
    };

    // The admin aggregate backs three Control ports (profiles / Agent resource
    // bindings / webhooks) off one store.
    let mut admin_profiles: Option<Arc<dyn awaken_admin_config_api::InferenceProfileStore>> = None;
    let mut admin_webhooks: Option<Arc<dyn awaken_admin_config_api::WebhookStore>> = None;
    let mut admin_resources: Option<Arc<dyn awaken_config_resolver::AgentInputBindingRepository>> =
        None;
    if opens_control {
        ensure_parent(&cfg.admin)?;
        match &cfg.admin {
            StoreBackend::Sqlite(p) => {
                let admin = awaken_admin_config_api::SqliteAdminStore::open(&path(p))
                    .map_err(|error| format!("open admin SQLite {}: {error}", p.display()))?;
                let admin = Arc::new(admin);
                admin_profiles = Some(admin.clone());
                admin_resources = Some(admin.clone());
                admin_webhooks = Some(admin);
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
                admin_profiles = Some(admin.clone());
                admin_resources = Some(admin.clone());
                admin_webhooks = Some(admin);
            }
        }
    }

    let coordinator = if role_owns_managed_execution(role) {
        ensure_parent(&cfg.sessions)?;
        let sessions: Arc<dyn awaken_session_contract::ManagedSessionRepository>;
        let deployments: Arc<dyn awaken_protocol_managed::DeploymentRepository>;
        let memory_extractions: Arc<dyn awaken_protocol_managed::MemoryExtractionRepository>;
        let dream_repository: Arc<dyn awaken_protocol_managed::DreamRepository>;
        match &cfg.sessions {
            StoreBackend::Sqlite(p) => {
                let repository = Arc::new(
                    awaken_session_store::SqliteManagedSessionRepository::open(&path(p)).map_err(
                        |error| format!("open sessions SQLite {}: {error}", p.display()),
                    )?,
                );
                sessions = repository.clone();
                deployments = repository.clone();
                memory_extractions = repository.clone();
                dream_repository = repository;
            }
            StoreBackend::Postgres(url) => {
                let repository = Arc::new(match postgres_schema {
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
                .map_err(|error| format!("connect sessions Postgres: {error}"))?);
                sessions = repository.clone();
                deployments = repository.clone();
                memory_extractions = repository.clone();
                dream_repository = repository;
            }
        }
        Some(CoordinatorStores {
            resource_component: resource_component
                .ok_or_else(|| "Coordinator stores require Resources component".to_owned())?,
            sessions,
            deployments,
            memory_extractions,
            dream_repository,
        })
    } else {
        None
    };

    let config: Option<Arc<dyn awaken_config_store::ScopedConfigRegistry>> = if opens_control {
        ensure_parent(&cfg.config)?;
        Some(match &cfg.config {
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
        })
    } else {
        None
    };

    // Environment definitions and execution work have an explicit backend instead
    // of borrowing Session's address. Split Control and Coordinator therefore open
    // one shared registry without giving Control a Session/Deployment repository.
    let environments: Arc<EnvironmentState> = if open_environment_stores {
        ensure_parent(&cfg.environments)?;
        match &cfg.environments {
            StoreBackend::Sqlite(environment_path) => Arc::new(
                EnvironmentState::with_stores(
                    Arc::new(
                        awaken_env_store::SqliteEnvRegistry::open(&path(environment_path))
                            .map_err(|error| format!("open environments SQLite: {error}"))?,
                    ),
                    Arc::new(
                        awaken_work_store::SqliteWorkQueue::open(&path(
                            &environment_path.with_file_name("work_queue.db"),
                        ))
                        .map_err(|error| format!("open work queue SQLite: {error}"))?,
                    ),
                )
                .with_sandbox_policies(Arc::new(
                    awaken_sandbox_policy_store::SqliteSandboxExecutionPolicyStore::open(&path(
                        &environment_path.with_file_name("sandbox_policies.db"),
                    ))
                    .map_err(|error| format!("open sandbox policies SQLite: {error}"))?,
                )),
            ),
            StoreBackend::Postgres(url) => {
                let environments = match postgres_schema {
                    PostgresSchemaMode::Migrate => {
                        awaken_env_store::PostgresEnvRegistry::connect(url).await
                    }
                    PostgresSchemaMode::Verify => {
                        awaken_env_store::PostgresEnvRegistry::connect_existing(url).await
                    }
                }
                .map_err(|error| format!("connect environments Postgres: {error}"))?;
                let work = match postgres_schema {
                    PostgresSchemaMode::Migrate => {
                        awaken_work_store::PostgresWorkQueue::connect(url).await
                    }
                    PostgresSchemaMode::Verify => {
                        awaken_work_store::PostgresWorkQueue::connect_existing(url).await
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
        }
    } else {
        Arc::new(EnvironmentState::new())
    };

    Ok(ProcessStores {
        workspace_root: Some(workspace_root),
        control: if opens_control {
            Some(ControlStores {
                catalog: catalog.expect("Control role opens Catalog"),
                credentials: credentials.expect("Control role opens CredentialRepo"),
                secrets: secrets.expect("Control role opens SecretStore"),
                profiles: admin_profiles.expect("Control role opens profile store"),
                resources: admin_resources.expect("Control role opens Resource authoring store"),
                webhooks: admin_webhooks.expect("Control role opens Webhook store"),
                config: config.expect("Control role opens Config store"),
            })
        } else {
            None
        },
        coordinator,
        environments,
    })
}

/// Local SQLite is a backend selection, not a second store assembly.
async fn open_local_process_stores(
    dir: &std::path::Path,
    key: &[u8; 32],
) -> Result<ProcessStores, String> {
    let resource_component = open_resource_component(
        config::ResourcePlaneStoreBackend::Embedded(dir.to_path_buf()),
        PostgresSchemaMode::Migrate,
    )
    .await?;
    open_process_stores(
        awaken_control::ControlStoreConfig::local(dir),
        Some(resource_component),
        dir.to_path_buf(),
        Some(key),
        config::Role::AllInOne,
        PostgresSchemaMode::Migrate,
        true,
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
    assemble_runtime_process_router(
        in_memory_process_stores(),
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
    build_runtime_process_assembly(deployment, Some(key), config::Role::AllInOne).await
}

/// Coordinator-only process assembly. It reuses the exact same Session, Run,
/// Dispatch, and Worker-coordination construction as all-in-one, while the
/// shared role selector keeps Control routes outside the exposed API.
pub async fn build_coordinator_assembly(
    deployment: &config::ResolvedDeployment,
) -> Result<ProcessAssembly, String> {
    build_runtime_process_assembly(deployment, None, config::Role::Coordinator).await
}

async fn build_runtime_process_assembly(
    deployment: &config::ResolvedDeployment,
    key: Option<&[u8; 32]>,
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
    let resource_component =
        open_resource_component(deployment.resources.clone(), postgres_schema).await?;
    let stores = open_process_stores(
        deployment.control.clone(),
        Some(resource_component),
        deployment.data_dir.clone(),
        key,
        role,
        postgres_schema,
        true,
    )
    .await?;
    let hand_executors =
        awaken_server::placement::connect_declared_hands(&deployment.hand_connections).await?;
    let executable_agent_wiring =
        executable_agent_registration::for_runtime_role(role, deployment, postgres_schema).await?;
    let worker_authenticator = worker_transport_security::authenticator(deployment)?;
    let control_service = if role == config::Role::Coordinator {
        let (url, token) = deployment.control_service.coordinator_credentials()?;
        Some(ControlServicePorts::remote(Arc::new(
            awaken_server::control_service_boundary::HttpControlServiceClient::new(url, token)?,
        )))
    } else {
        None
    };
    let router = assemble_runtime_process_router(
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
            model_supply: local_model_supply(deployment.cloud_models.is_enabled()),
            brokered_catalog: None,
            local_acp_observations: deployment.local_acp_observations.clone(),
            hand_executors,
            web_search_providers: None,
            web_search_publication_resolver: None,
            executable_agent_wiring: Some(executable_agent_wiring),
            worker_authenticator: Some(worker_authenticator),
            control_service_token: None,
            control_service,
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
    key: Option<&[u8; 32]>,
) -> Result<(), String> {
    let manifest = migration_manifest(deployment.role);
    let resource_component = if manifest.contains(&MigrationComponent::Resources) {
        Some(
            open_resource_component(deployment.resources.clone(), PostgresSchemaMode::Migrate)
                .await?,
        )
    } else {
        None
    };
    if manifest.contains(&MigrationComponent::Control)
        || manifest.contains(&MigrationComponent::Coordinator)
    {
        open_process_stores(
            deployment.control.clone(),
            resource_component,
            deployment.data_dir.clone(),
            key,
            deployment.role,
            PostgresSchemaMode::Migrate,
            manifest.contains(&MigrationComponent::Coordinator),
        )
        .await
        .map(drop)?;
    }
    if manifest.contains(&MigrationComponent::ExecutableAgentCatalog) {
        executable_agent_registration::migrate(deployment).await?;
    }
    if manifest.contains(&MigrationComponent::Coordinator) {
        awaken_server::migrate_postgres_coordinator_schema(&deployment.runtime).await?;
    }
    Ok(())
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
    let resource_component = open_resource_component(deployment.resources.clone(), postgres_schema)
        .await
        .unwrap_or_else(|error| panic!("open resource stores: {error}"));
    let stores = open_process_stores(
        deployment.control.clone(),
        Some(resource_component),
        deployment.data_dir.clone(),
        Some(&key),
        config::Role::AllInOne,
        postgres_schema,
        true,
    )
    .await
    .unwrap_or_else(|error| panic!("open deployment stores: {error}"));
    let hand_executors =
        awaken_server::placement::connect_declared_hands(&deployment.hand_connections)
            .await
            .unwrap_or_else(|error| panic!("declared Hand topology: {error}"));
    assemble_runtime_process_router(
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
            model_supply: local_model_supply(deployment.cloud_models.is_enabled()),
            brokered_catalog: None,
            local_acp_observations: deployment.local_acp_observations,
            hand_executors,
            web_search_providers: None,
            web_search_publication_resolver: None,
            executable_agent_wiring: None,
            worker_authenticator: None,
            control_service_token: None,
            control_service: None,
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
    assemble_runtime_process_router(
        in_memory_process_stores(),
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
    assemble_runtime_process_router(
        open_local_process_stores(dir, key)
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
    assemble_runtime_process_router(
        in_memory_process_stores(),
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
    assemble_runtime_process_router(
        open_local_process_stores(dir, key)
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
    let router = assemble_runtime_process_router(
        open_local_process_stores(dir, key)
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

fn brokered_inference_client(
    cloud_models_enabled: bool,
    remote_iam: Option<&Arc<RemoteManagementAuthz>>,
    cloud_api_base_url: Option<&str>,
    execution_workspace: &str,
) -> Option<Arc<awaken_server::brokered_inference::HttpBrokeredInferenceClient>> {
    cloud_models_enabled
        .then(|| remote_iam.and_then(|authz| authz.cloud_user_token()))
        .flatten()
        .map(|token| {
            let base_url = cloud_api_base_url
                .expect("Awaken Cloud identity requires a Cloud inference API URL");
            Arc::new(
                awaken_server::brokered_inference::HttpBrokeredInferenceClient::new(
                    base_url,
                    token,
                    execution_workspace,
                )
                .unwrap_or_else(|error| panic!("Cloud inference configuration: {error}")),
            )
        })
}

fn publication_model_assembly(
    composition: PublicationModelComposition,
    stores: &ProcessStores,
    cloud_models_enabled: bool,
) -> PublicationModelAssembly {
    let control = stores
        .control
        .as_ref()
        .expect("model publication requires Control stores");
    let executor_model_capabilities =
        Arc::new(awaken_server::model_directory::installed_executor_model_capabilities());
    match composition {
        PublicationModelComposition::PublishedProviders => PublicationModelAssembly {
            publication_resolver: Arc::new(
                awaken_server::model_resolver::CatalogModelPublicationResolver::from_repo(
                    control.catalog.clone(),
                    control.credentials.clone(),
                )
                .with_executor_capabilities(executor_model_capabilities)
                .with_profiles(control.profiles.clone())
                .with_worker_directory(awaken_server::worker_directory())
                .with_brokered_access(cloud_models_enabled),
            ),
            runtime: RuntimeModelAssembly::PublishedProviders,
        },
        PublicationModelComposition::HostedPublication { resolver } => PublicationModelAssembly {
            publication_resolver: resolver,
            runtime: RuntimeModelAssembly::NoModelConfigured,
        },
        PublicationModelComposition::Host { executor, binding } => {
            let model_ref = binding.model_ref.clone();
            PublicationModelAssembly {
                publication_resolver: Arc::new(ExactHostModelPublicationResolver { binding }),
                runtime: RuntimeModelAssembly::Host {
                    executor,
                    model_ref,
                },
            }
        }
    }
}

fn runtime_model_wiring(
    runtime: RuntimeModelAssembly,
    credential_materializer: &awaken_credential_materializer::PinnedCredentialMaterializer,
    cloud_models_enabled: bool,
    brokered_client: Option<&Arc<awaken_server::brokered_inference::HttpBrokeredInferenceClient>>,
) -> RuntimeModelWiring {
    match runtime {
        RuntimeModelAssembly::PublishedProviders => RuntimeModelWiring {
            executor: Arc::new(awaken_runtime_host::NoModelConfiguredExecutor),
            model_ref: awaken_runtime_host::UNCONFIGURED_MODEL_REF.to_string(),
            materializer: Some(Arc::new({
                let materializer = awaken_server::inference_materializer::CredentialInferenceMaterializer::from_pinned(
                    credential_materializer.clone(),
                )
                .with_brokered_mode(cloud_models_enabled);
                match brokered_client {
                    Some(client) => materializer.with_brokered_client(client.clone()),
                    None => materializer,
                }
            })),
        },
        RuntimeModelAssembly::NoModelConfigured => RuntimeModelWiring {
            executor: Arc::new(awaken_runtime_host::NoModelConfiguredExecutor),
            model_ref: awaken_runtime_host::UNCONFIGURED_MODEL_REF.to_string(),
            materializer: None,
        },
        RuntimeModelAssembly::Host {
            executor,
            model_ref,
        } => RuntimeModelWiring {
            executor,
            model_ref,
            materializer: None,
        },
    }
}

/// Assemble Coordinator, optionally composing the canonical Control component
/// for AllInOne. The data plane comes from [`awaken_server::mount_with_managed`];
/// this process layer merges routers and supervises lifecycle without rebuilding
/// either domain application.
async fn assemble_runtime_process_router(
    stores: ProcessStores,
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
    debug_assert!(matches!(
        role,
        config::Role::AllInOne | config::Role::Coordinator
    ));
    let worker_authenticator = assembly.worker_authenticator.unwrap_or_else(|| {
        Arc::new(awaken_worker_transport_security::HeaderWorkerAuthenticator)
            as Arc<dyn awaken_worker_transport_security::WorkerRequestAuthenticator>
    });
    let (
        executable_agent_catalog,
        executable_agent_registrar,
        executable_agent_private_router,
        executable_agent_projection_refresher,
    ) = executable_agent_registration::process_parts(assembly.executable_agent_wiring);
    let deployment = assembly.deployment;
    let hand_executors = assembly.hand_executors;
    let cloud_api_base_url = assembly.cloud_api_base_url;
    let model_supply = assembly.model_supply.clone();
    let cloud_models_enabled = model_supply.cloud_models_enabled;
    let injected_brokered_catalog = assembly.brokered_catalog.clone();
    let org_id = assembly.org_id.unwrap_or_else(local_org_id);
    let managed_rate_limiter =
        Arc::new(awaken_protocol_managed::ManagedRateLimiter::for_organization(org_id.clone()));
    let mcp_bearer_token = assembly.mcp_bearer_token;
    // Cause/effect composition rule: one selected ResourceComponent is moved intact
    // into the Host. The management Skill API borrows the one additional view it
    // needs; no tuple decomposition or parallel Resources reconstruction.
    let coordinator_stores = stores
        .coordinator
        .as_ref()
        .expect("Managed Execution role requires Coordinator stores");
    let resource_component = &coordinator_stores.resource_component;
    // Resolve the installation's Workspace exactly once, then inject the same
    // coordinate into every adapter assembled below. Durable roots persist it;
    // ephemeral roots receive a process-local generated coordinate.
    let platform_workspace = stores.workspace_root.as_deref().map_or_else(
        SharedHost::provision_local_workspace,
        SharedHost::provision_local_workspace_at,
    );
    let brokered_client = brokered_inference_client(
        cloud_models_enabled,
        remote_iam.as_ref(),
        cloud_api_base_url.as_deref(),
        &platform_workspace,
    );
    let credential_materializer = (role == config::Role::AllInOne)
        .then(|| {
            stores.control.as_ref().map(|control| {
                awaken_credential_materializer::PinnedCredentialMaterializer::new(
                    control.credentials.clone(),
                    control.secrets.clone(),
                )
            })
        })
        .flatten();
    let web_search_providers = assembly
        .web_search_providers
        .unwrap_or_else(awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins);
    let model_assembly = (role == config::Role::AllInOne)
        .then(|| publication_model_assembly(model_composition, &stores, cloud_models_enabled));
    let model_wiring = match (&model_assembly, &credential_materializer) {
        (Some(assembly), Some(credentials)) => runtime_model_wiring(
            assembly.runtime.clone(),
            credentials,
            cloud_models_enabled,
            brokered_client.as_ref(),
        ),
        (None, None) => RuntimeModelWiring {
            executor: Arc::new(awaken_runtime_host::NoModelConfiguredExecutor),
            model_ref: awaken_runtime_host::UNCONFIGURED_MODEL_REF.to_string(),
            materializer: None,
        },
        _ => unreachable!("Control model adapters and Control stores are composed together"),
    };
    let web_search_publication_resolver =
        assembly.web_search_publication_resolver.unwrap_or_else(|| {
            Arc::new(awaken_config_service::WebSearchPublicationResolver::new(
                web_search_providers.clone(),
            ))
        });
    // Keep the IAM handles for the sibling resource PEP. The authoring router owns
    // its PEP; File/Memory/Skill routes are wrapped independently after the data
    // router is assembled, so neither plane depends on the other's services.
    let resource_iam = iam.clone();
    let resource_remote_iam = remote_iam.clone();
    let deployment_iam = iam.clone();
    let deployment_remote_iam = remote_iam.clone();
    let live_runtime_capabilities = stores.control.as_ref().map(|control| {
        Arc::new(LiveRuntimeCapabilities {
            initial: assembly.local_acp_observations.clone(),
            workers: awaken_server::worker_directory(),
            credentials: control.credentials.clone(),
            workspace: platform_workspace.clone(),
        })
    });
    // Control owns this complete component. Standalone Control and AllInOne
    // supply different process adapters but call the same domain builder;
    // Coordinator never constructs ConfigService or a Control router.
    let control_component = match role {
        config::Role::AllInOne => Some(
            control_component_for_process(
                &stores,
                &platform_workspace,
                executable_agent_registrar,
                model_assembly
                    .as_ref()
                    .expect("AllInOne composes model publication")
                    .publication_resolver
                    .clone(),
                web_search_publication_resolver,
                &web_search_providers,
                brokered_client.clone(),
                injected_brokered_catalog,
                model_supply,
                &assembly.local_acp_observations,
                live_runtime_capabilities
                    .clone()
                    .expect("AllInOne composes Control runtime capabilities"),
                Some(Arc::new(awaken_control::HostResourceInventory::new(
                    resource_component.resource_catalog(),
                    resource_component.skill_store(),
                    &platform_workspace,
                ))),
                iam.clone(),
                local_browser_auth,
                remote_iam.clone(),
            )
            .await,
        ),
        config::Role::Coordinator => None,
        config::Role::Control | config::Role::Worker => {
            unreachable!("runtime process assembly accepts only AllInOne or Coordinator")
        }
    };
    let ProcessStores {
        workspace_root: _,
        control: control_stores,
        coordinator,
        environments: env_state,
    } = stores;
    let CoordinatorStores {
        resource_component,
        sessions,
        deployments,
        dream_repository,
        memory_extractions,
    } = coordinator.expect("Managed Execution role requires Coordinator stores");
    let resource_catalog = resource_component.resource_catalog();
    let (
        control,
        mcp_export,
        reconciler,
        admin_execs,
        credential_source,
        deployment_audit_plane,
        local_webhook_stores,
    ) = match control_component {
        Some(component) => {
            let control_stores = control_stores
                .as_ref()
                .expect("AllInOne Control component has Control stores");
            let mcp_export = awaken_server::mcp_export::router(
                awaken_admin_assistant::admin_tool_descriptors(),
                component.admin_tools.clone(),
                mcp_bearer_token,
            );
            (
                component.router,
                mcp_export,
                Some(component.publication_reconciler),
                component.admin_tools,
                component.vault_state.clone()
                    as Arc<dyn awaken_protocol_managed::SessionCredentialSource>,
                component.management_audit,
                Some((
                    control_stores.webhooks.clone(),
                    control_stores.secrets.clone(),
                )),
            )
        }
        None => {
            let ports = assembly
                .control_service
                .clone()
                .expect("split Coordinator requires Control service ports");
            (
                Router::new(),
                Router::new(),
                None,
                Vec::new(),
                ports.credentials,
                ManagementAuditPlane::from_repository(ports.audit),
                None,
            )
        }
    };

    // Only Coordinator and AllInOne can reach this point. Their one execution
    // store group owns Session, Deployment/Run, Dream, extraction work and the
    // lifecycle outbox; Control has no fallback implementation of that group.
    // Application credentials and the protocol guards that consume them must
    // live in the same Coordinator process. The issuer validates bindings
    // against this Coordinator's sole Managed Session repository; split Control
    // neither mirrors that repository nor owns a second token directory.
    let application_access = Arc::new(awaken_authz_enforce::ApplicationAccessStore::new());
    let webhook_sink: Arc<dyn awaken_session_contract::SessionLifecycleSink> =
        match local_webhook_stores {
            Some((webhook_store, secrets)) => {
                awaken_webhook_managed::assemble_with_session_repo(
                    webhook_store,
                    secrets,
                    Some(org_id.clone()),
                    sessions.clone(),
                )
                .0
            }
            None => Arc::new(awaken_webhook_managed::WebhookLifecycleSink::with_delivery(
                assembly
                    .control_service
                    .as_ref()
                    .expect("split Coordinator requires Control webhook delivery")
                    .webhooks
                    .clone(),
                sessions.clone(),
            )),
        };
    // The data plane: the host runs the server model, resolves a session's agent to
    // its installed config, and carries the management tool executables so the
    // reserved-scope assistant can call them. It shares the SAME skill store and
    // Resource Catalog the capability inventory reads, so a skill or memory store the
    // host serves is exactly what the assistant enumerates, and identity survives a
    // restart.
    let mut host_builder = match deployment {
        Some(deployment) => SharedHost::new_with_resource_component_and_deployment(
            model_wiring.executor,
            model_wiring.model_ref,
            resource_component,
            deployment,
        ),
        None => SharedHost::new_with_resource_component(
            model_wiring.executor,
            model_wiring.model_ref,
            resource_component,
        ),
    };
    host_builder = host_builder
        .with_local_workspace(platform_workspace.clone())
        .with_web_search_provider_registry(web_search_providers)
        .with_acp_tool_exporter(Arc::new(awaken_server::mcp_export::SessionToolExporter))
        .with_remote_attempt_executor(awaken_server::a2a_attempt_executor(
            credential_materializer.clone(),
        ))
        .with_agent_publications(executable_agent_catalog.clone())
        .with_agent_resource_references(executable_agent_catalog.clone())
        .with_admin_tools(admin_execs);
    if let Some(credentials) = credential_materializer.clone() {
        host_builder = host_builder.with_credential_materializer(credentials);
    }
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
        .with_acp_from_deployment(credential_materializer.clone())
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
    let mut managed_host =
        ManagedHost::new(host.clone()).with_resource_validator(resource_catalog.clone());
    if let Some(credentials) = credential_materializer {
        managed_host = managed_host.with_credential_materializer(credentials);
    }
    let mut managed_state = ManagedState::new_with_mcp(managed_host)
        .with_credential_source(credential_source)
        .with_environments(env_state.clone())
        .with_resource_catalog(resource_catalog.clone())
        .with_resource_purge_scheduler(host.clone())
        // Share the SAME config plane `/v1/agents` reads, so a session inheriting a
        // published agent's model sees the authoritative config-plane truth (M2).
        .with_config_source(executable_agent_catalog.clone())
        .with_session_repo(sessions.clone());
    managed_state = managed_state.with_lifecycle_sink(webhook_sink);
    let managed_state = Arc::new(managed_state);
    // Workspace path addressing (ADR-0048 D3 / ADR-0051): wrap the fully-merged flat
    // surface so a `/v1/workspaces/{ws}/…` request is captured, rewritten to its flat
    // `/v1/…` form, and its `{ws}` stamped as the edge scope before it re-enters
    // routing. Flat requests fall through unchanged. The same assembly returns the
    // DreamState it mounted, so scheduling cannot target a parallel instance.
    let model_directory: Arc<dyn awaken_server::ModelDirectory> = match control_stores.as_ref() {
        Some(control) => Arc::new(
            awaken_server::model_directory::CatalogModelDirectory::with_source(
                control.catalog.clone(),
                control.credentials.clone(),
                live_runtime_capabilities.expect("AllInOne composes live Control capabilities"),
            ),
        ),
        None => Arc::new(
            awaken_server::model_directory::ExecutableAgentModelDirectory::new(
                executable_agent_catalog.clone(),
            ),
        ),
    };
    let registration_router = executable_agent_registration::layer_refresh(
        executable_agent_private_router,
        executable_agent_projection_refresher,
    );
    let coordinator =
        awaken_server::build_coordinator_component(awaken_server::CoordinatorDependencies {
            host,
            managed_state,
            resource_catalog,
            application_access,
            model_directory,
            dream_repository,
            worker_authenticator,
            deployment_repository: deployments,
            executable_agents: executable_agent_catalog,
            rate_limiter: managed_rate_limiter.clone(),
            environments: env_state,
            sessions,
            default_workspace: platform_workspace.clone(),
            registration_router,
        })
        .await
        .unwrap_or_else(|error| panic!("build Coordinator component: {error}"));
    let coordinator_management = awaken_control::protect_management_router(
        coordinator.management_router,
        deployment_audit_plane,
        deployment_iam,
        deployment_remote_iam,
    );
    let mut data = coordinator.router.merge(coordinator_management);
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
        reconciler,
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
    use awaken_config_service::ModelPublicationResolver;
    use awaken_protocol_managed::{
        ApplicationContributionState, ControlSessionCreationInputs, EnvironmentFingerprint,
        EnvironmentRevision, EnvironmentSnapshot, IdempotencyRecord, PersistedSession,
        SessionBaselineState, SessionCreationIntent, SessionNetworkPolicy, stable_fingerprint,
    };

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
            tools: Default::default(),
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
            let stores = process_stores_for_runtime_storage(Some(dir.path()));
            let execution = stores
                .coordinator
                .as_ref()
                .expect("test composition owns Managed Execution");
            let value = session("sesn-restart");
            let payload_hash = stable_fingerprint(&value);
            execution
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

        let reopened = process_stores_for_runtime_storage(Some(dir.path()));
        let execution = reopened
            .coordinator
            .as_ref()
            .expect("test composition owns Managed Execution");
        assert_eq!(
            execution.sessions.owner("sesn-restart").await.as_deref(),
            Some("workspace-a")
        );
        assert_eq!(
            execution
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
        // the repository-backed Deployment component is assembled.
        let dir = tempfile::tempdir().expect("temporary runtime storage");
        let durable = process_stores_for_runtime_storage(Some(dir.path()));
        let durable = durable
            .coordinator
            .as_ref()
            .expect("durable test composition owns Managed Execution");
        assert_eq!(
            Arc::as_ptr(&durable.sessions) as *const (),
            Arc::as_ptr(&durable.deployments) as *const (),
            "D1"
        );

        let ephemeral = process_stores_for_runtime_storage(None);
        let ephemeral = ephemeral
            .coordinator
            .as_ref()
            .expect("ephemeral test composition owns Managed Execution");
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

    #[test]
    fn control_component_has_exactly_the_authoring_process_owners() {
        // Cause/effect decision table:
        // R1 Control and R2 AllInOne contain the authoring authority, so they
        // build the canonical Control component (including Vault recovery).
        // R3 Coordinator and R4 Worker consume only explicit projections and
        // must not construct a parallel ConfigService or Control router.
        assert!(role_owns_control_component(config::Role::Control), "R1");
        assert!(role_owns_control_component(config::Role::AllInOne), "R2");
        assert!(
            !role_owns_control_component(config::Role::Coordinator),
            "R3"
        );
        assert!(!role_owns_control_component(config::Role::Worker), "R4");
    }

    #[test]
    fn managed_execution_stores_have_only_execution_process_owners() {
        // Cause/effect decision table:
        // R1 Coordinator owns the distributed execution boundary and R2
        // AllInOne composes it locally, so both open the Session/Deployment store
        // group. R3 Control and R4 Worker must never create a second local group.
        assert!(role_owns_managed_execution(config::Role::Coordinator), "R1");
        assert!(role_owns_managed_execution(config::Role::AllInOne), "R2");
        assert!(!role_owns_managed_execution(config::Role::Control), "R3");
        assert!(!role_owns_managed_execution(config::Role::Worker), "R4");
    }

    #[test]
    fn resource_component_has_only_resource_serving_process_owners() {
        // Cause/effect decision table:
        // R1 AllInOne and R2 Coordinator serve claim-fenced Resource APIs and
        // therefore compose the canonical component. R3 Control consumes only
        // Resource authoring/read ports; R4 Worker consumes authenticated per-kind
        // clients, so neither may open a Resource authority component.
        assert!(
            role_composes_resource_component(config::Role::AllInOne),
            "R1"
        );
        assert!(
            role_composes_resource_component(config::Role::Coordinator),
            "R2"
        );
        assert!(
            !role_composes_resource_component(config::Role::Control),
            "R3"
        );
        assert!(
            !role_composes_resource_component(config::Role::Worker),
            "R4"
        );
    }

    /// Cause/effect decision table:
    ///
    /// | role | authoring API | Session API | Deployment API | registration API |
    /// | --- | --- | --- | --- | --- |
    /// | Control | mounted | absent | absent | absent |
    /// | Coordinator | absent | mounted | mounted | authenticated |
    ///
    /// AllInOne merging is covered by the existing full-surface integration
    /// suites; this test owns the two exclusion rules that those suites cannot
    /// prove.
    #[tokio::test]
    async fn service_roles_expose_only_their_owned_api() {
        let app = assemble_control_process_router(
            in_memory_control_stores(),
            None,
            None,
            None,
            PublicationModelComposition::PublishedProviders,
            ProcessAssemblyOptions {
                role: config::Role::Control,
                ..Default::default()
            },
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

        let deployment = app
            .clone()
            .oneshot(Request::get("/v1/deployments").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(deployment.status(), StatusCode::NOT_FOUND);

        let session = app
            .oneshot(Request::get("/v1/sessions").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(session.status(), StatusCode::NOT_FOUND);

        let (coordinator_stores, control_service) = in_memory_split_coordinator();
        let app = assemble_runtime_process_router(
            coordinator_stores,
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
                control_service: Some(control_service),
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

        let deployments = app
            .clone()
            .oneshot(Request::get("/v1/deployments").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(deployments.status(), StatusCode::OK);

        let retired_private_launch = app
            .oneshot(
                Request::post("/internal/v1/deployment-sessions/launch")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(retired_private_launch.status(), StatusCode::NOT_FOUND);
    }

    #[derive(Debug)]
    struct RecordingHostedResolver {
        called: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl awaken_config_service::ModelPublicationResolver for RecordingHostedResolver {
        async fn resolve_models(
            &self,
            _workspace: &awaken_tenancy::ScopeId,
            _selection: &awaken_config_store::ModelSelection,
            _candidates: &[awaken_runtime_contract::resolved::ModelBinding],
        ) -> Result<
            awaken_config_service::ResolvedPublicationModels,
            awaken_config_service::PublicationResolutionError,
        > {
            self.called.store(true, Ordering::SeqCst);
            Err(awaken_config_service::PublicationResolutionError::MissingPrimary)
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
        let app = assemble_control_process_router(
            in_memory_control_stores(),
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
