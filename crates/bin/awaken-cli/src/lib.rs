//! `awaken-cli` library: product-process startup.
//!
//! Control (`awaken-control`) and Coordinator (`awaken-coordinator`) do not depend on
//! each other. This crate starts their product processes: it opens deployment
//! stores, builds shared adapters, asks each owner for its router, and exposes
//! exactly the API selected by `config::Role`. AllInOne merges those same routers;
//! it does not maintain a second implementation. The role-named Control and
//! Coordinator executables terminate in the same lifecycle, while Worker keeps
//! its independent authority-free bootstrap.
//!
//! Every binary target is a thin shell over this library.
mod acp_local_credentials;
mod assistant_selection;
pub mod config;
pub mod console;
mod console_assets;
mod control;
mod control_component;
mod credential_probe;
mod deployment_process;
#[cfg(any(test, feature = "test-support"))]
mod exact_host_model;
mod executable_agent_registration;
mod executable_environment_registration;
mod executable_projection_refresh;
mod identity;
#[cfg(any(test, feature = "test-support"))]
mod local_process_stores;
mod observation_reconcile;
mod process_admin;
mod process_startup;
mod process_stores;
mod process_surface;
mod resources;
mod runtime_process_router;
mod service;
mod web_search_publication;
mod worker_observation_wiring;
mod worker_transport_security;

use std::sync::Arc;
use std::{future::Future, mem};

use awaken_config_service::ManagementAuditPlane;
use awaken_protocol_managed::ManagedState;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_host::{ExtMcpProbe, ManagedHost, SharedHost};
use axum::Router;
#[cfg(any(test, feature = "test-support"))]
use exact_host_model::ExactHostModelPublicationResolver;
#[cfg(any(test, feature = "test-support"))]
use local_process_stores::in_memory_process_stores;
#[cfg(any(test, feature = "test-support"))]
use local_process_stores::open_local_process_stores;
#[cfg(test)]
use local_process_stores::{
    in_memory_control_stores, in_memory_split_coordinator, process_stores_for_runtime_storage,
};

pub use crate::process_admin::{
    DrainController, process_admin_router, register_active_streams_gauge, with_connection_metric,
    with_process_admin,
};
pub use acp_local_credentials::{
    AcpLocalCredentialResolver, PreparedLocalAcp, PreparedLocalWorker, local_acp_diagnostics,
    prepare_local_acp, prepare_local_worker, registered_memory_mounter_factory,
};
pub use console_assets::mount as mount_console;
pub use console_assets::mount_with_navigation as mount_console_with_navigation;
pub use control::{
    build_control_router, build_control_router_with_publication_resolver,
    build_control_router_with_publication_resolver_and_web_search, prepare_control_process,
    prepare_control_process_with_managed_services,
    prepare_control_process_with_publication_resolver,
    prepare_control_process_with_publication_resolver_and_web_search,
    prepare_control_process_with_publication_resolver_web_search_and_lifecycle_delivery,
};
use control_component::{control_component_for_process, prepare_control_routers};
pub use deployment_process::migrate_deployment_schema;
use deployment_process::{
    prepare_runtime_process, prepare_runtime_process_with_coordinator_services,
};
use identity::identity_wiring;
pub use managed_platform::{
    CoordinatorServiceAdapters, ManagedBackgroundService, ManagedServiceAdapters,
};
use process_startup::{ProcessStartup, local_model_supply};
#[cfg(test)]
use process_stores::role_hosts_resources;
use process_stores::{
    ControlStores, CoordinatorStores, MigrationComponent, PostgresSchemaMode, ProcessStores,
    migration_manifest, role_owns_control_component, role_owns_managed_execution,
};
#[cfg(any(test, feature = "test-support"))]
use resources::ephemeral_resources_application;
use resources::open_resources_application;
use runtime_process_router::prepare_runtime_routers;
pub use service::{
    ServiceRole, migrate_service, run_all_in_one_with_services, run_service, run_service_binary,
    serve_prepared_control, serve_prepared_coordinator,
};

/// Run a service future on the canonical process runtime.
///
/// Credential-backed Managed MCP realization crosses the durable ingress,
/// materializer, and connector stacks in one poll. The Tokio default worker
/// stack (2 MiB) is insufficient for that valid debug/recovery path and aborts
/// the whole process before an error can be projected. Keep one explicit
/// process-level stack budget for every launcher instead of relying on an
/// operator-only environment-variable workaround.
pub fn block_on_service<F: Future>(future: F) -> F::Output {
    const SERVICE_WORKER_STACK_BYTES: usize = 8 * 1024 * 1024;
    const { assert!(SERVICE_WORKER_STACK_BYTES >= 4 * 1024 * 1024) };
    const { assert!(SERVICE_WORKER_STACK_BYTES.is_multiple_of(mem::size_of::<usize>())) };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("awaken-runtime")
        .thread_stack_size(SERVICE_WORKER_STACK_BYTES)
        .build()
        .expect("build Awaken service runtime")
        .block_on(future)
}
// Embedded management-plane IAM (ADR-0042/0043 P1) + the mint spec and bootstrap
// constants a test / operator embedding drives — re-exported from the authoring plane.
pub use awaken_control::{
    ADMIN_TOKEN_FILE, BOOTSTRAP_PRINCIPAL, BOOTSTRAP_WORKSPACE, ManagementAuthz,
    ManagementIdentityMode, RemoteManagementAuthz, TokenSpec, embedded_iam,
};
mod live_runtime_capabilities;
mod managed_platform;
use live_runtime_capabilities::LiveRuntimeCapabilities;

/// The two legal startup modes are deliberately disjoint: production
/// publishes catalog-backed provider candidates and installs their credential
/// materializer; deterministic scenarios publish one exact host executor and do
/// not install a provider materializer.
enum PublicationModelSupply {
    PublishedProviders,
    /// A hosted startup owns provider custody and injects its resolver into
    /// the same Awaken publication pipeline. This variant is legal only for the
    /// control-only surface: the separate hosted Worker owns runtime
    /// materialization.
    HostedPublication {
        resolver: Arc<dyn awaken_config_service::ModelPublicationResolver>,
    },
    #[cfg(any(test, feature = "test-support"))]
    Host {
        executor: Arc<dyn LlmExecutor>,
        binding: awaken_runtime_contract::resolved::ModelBinding,
    },
}

impl PublicationModelSupply {
    fn needs_interactive_brokered_client(&self) -> bool {
        matches!(self, Self::PublishedProviders)
    }
}

/// Concrete wiring produced by one legal startup mode. Keeping these four
/// values together prevents a provider resolver from being paired with a host
/// executor or a Host publication from receiving a credential materializer.
struct RuntimeModelWiring {
    executor: Arc<dyn LlmExecutor>,
    model_ref: String,
    materializer:
        Option<Arc<dyn awaken_runtime_contract::inference::InferenceExecutorMaterializer>>,
}

/// Publication policy plus a deferred runtime choice. Standalone Control uses
/// only the resolver; execution adapters are realized only by runtime process.
struct ResolvedModelServices {
    publication_resolver: Arc<dyn awaken_config_service::ModelPublicationResolver>,
    runtime: RuntimeModelServices,
}

#[derive(Clone)]
enum RuntimeModelServices {
    PublishedProviders,
    NoModelConfigured,
    #[cfg(any(test, feature = "test-support"))]
    Host {
        executor: Arc<dyn LlmExecutor>,
        model_ref: String,
    },
}
#[derive(Clone)]
struct ControlServices {
    audit: Arc<dyn awaken_config_service::ManagementAuditRepository>,
    credentials: Arc<dyn awaken_session_application::SessionCredentialSource>,
    webhooks: Arc<dyn awaken_session_contract::LifecycleFactDelivery>,
    consent: Arc<dyn awaken_runtime_contract::DataSubjectConsentSource>,
}

impl ControlServices {
    fn remote(
        client: Arc<awaken_coordinator::control_service_boundary::HttpControlServiceClient>,
    ) -> Self {
        Self {
            audit: client.clone(),
            credentials: client.clone(),
            webhooks: client.clone(),
            consent: client,
        }
    }
}
/// Role-owned HTTP surfaces plus the cleartext local setup handoff printed by
/// the CLI once. `private_router` is never merged into `public_router`.
#[derive(Clone)]
pub struct CoordinatorAuthorityHandles {
    pub runtime_authority: Arc<dyn awaken_runtime_host::RuntimeAuthority>,
    pub worker_directory: Arc<dyn awaken_worker_contract::WorkerDirectory>,
    pub sessions: Arc<dyn awaken_session_contract::ManagedSessionRepository>,
    /// The one process-owned Coordinator pool. Hosted adapters and the
    /// process-readiness probe clone this handle rather than reconnecting.
    pub postgres_pool: Option<sqlx::PgPool>,
    pub run_recovery:
        Option<Arc<dyn awaken_agent_contract::thread::read::recovery::RunRecoverySource>>,
    pub run_lifecycle:
        Option<Arc<dyn awaken_agent_contract::thread::read::lifecycle::RunLifecycleFeed>>,
}

pub struct PreparedProcess {
    pub public_router: Router,
    pub private_router: Router,
    pub local_setup: Option<awaken_control::LocalSetupHandoff>,
    pub registration_supervisor: Option<Arc<awaken_control::StaticRegistrationSupervisor>>,
    pub service_lifecycle: awaken_service_lifecycle::ServiceLifecycle,
    /// Canonical Coordinator persistence handles for a hosted composition.
    /// Consumers must reuse these handles and must not reopen the same stores.
    pub coordinator_authorities: Option<CoordinatorAuthorityHandles>,
    pub admin_tools: Vec<Arc<dyn awaken_runtime_contract::tool::RawTool>>,
}

struct ProcessRouters {
    public_router: Router,
    private_router: Router,
    registration_supervisor: Option<Arc<awaken_control::StaticRegistrationSupervisor>>,
    service_lifecycle: awaken_service_lifecycle::ServiceLifecycle,
    admin_tools: Vec<Arc<dyn awaken_runtime_contract::tool::RawTool>>,
}

impl ProcessRouters {
    fn new(
        public_router: Router,
        private_router: Router,
        registration_supervisor: Option<Arc<awaken_control::StaticRegistrationSupervisor>>,
        service_lifecycle: awaken_service_lifecycle::ServiceLifecycle,
        admin_tools: Vec<Arc<dyn awaken_runtime_contract::tool::RawTool>>,
    ) -> Self {
        // Router-only embedding helpers do not retain PreparedProcess. Keep the
        // same supervisor alive inside the router as well as exposing it to the
        // binary's readiness controller.
        let public_router = match &registration_supervisor {
            Some(supervisor) => public_router.layer(axum::Extension(supervisor.clone())),
            None => public_router,
        };
        let public_router = public_router.layer(axum::Extension(service_lifecycle.clone()));
        Self {
            public_router,
            private_router,
            registration_supervisor,
            service_lifecycle,
            admin_tools,
        }
    }
}

/// Open the canonical role-owned stores selected by
/// [`ControlStoreConfig`](awaken_control::ControlStoreConfig). The compatibility
/// bundle resolves backend addresses, but this function acquires only the group
/// owned by `role`; split Control and Coordinator never share authority stores.
struct ProcessStoreOpenOptions<'a> {
    control: awaken_control::ControlStoreConfig,
    coordinator: config::CoordinatorStoreConfig,
    resources: Option<awaken_resource_application::ResourcesApplication>,
    workspace_root: std::path::PathBuf,
    seal_key: Option<&'a [u8; 32]>,
    role: config::Role,
    postgres_schema: PostgresSchemaMode,
}

async fn open_process_stores(
    options: ProcessStoreOpenOptions<'_>,
) -> Result<ProcessStores, String> {
    use awaken_control::StoreBackend;

    let ProcessStoreOpenOptions {
        control: cfg,
        coordinator: coordinator_cfg,
        resources,
        workspace_root,
        seal_key: key,
        role,
        postgres_schema,
    } = options;

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
                awaken_model_catalog_store::SqliteCatalogRepo::open(&path(p))
                    .map_err(|error| format!("open catalog SQLite {}: {error}", p.display()))?,
            ),
            StoreBackend::Postgres(url) => Arc::new(
                match postgres_schema {
                    PostgresSchemaMode::Migrate => {
                        awaken_model_catalog_store::PostgresCatalogRepo::connect(url).await
                    }
                    PostgresSchemaMode::Verify => {
                        awaken_model_catalog_store::PostgresCatalogRepo::connect_existing(url).await
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
        let triple: (
            Arc<dyn awaken_credential_vault::repo::ManagedCredentialRepository>,
            Arc<dyn awaken_credential_vault::SecretStore>,
        ) = match &cfg.credential {
            StoreBackend::Sqlite(p) => {
                let file = path(p);
                let (creds, blobs) = awaken_credential_store::sqlite::open_migrated_pair(&file)
                    .map_err(|error| format!("open credential SQLite {}: {error}", p.display()))?;
                (
                    Arc::new(creds),
                    Arc::new(awaken_credential_store::SealedAeadSecretStore::over(
                        key,
                        Arc::new(blobs),
                    )),
                )
            }
            StoreBackend::Postgres(url) => {
                let (creds, blobs) = match postgres_schema {
                    PostgresSchemaMode::Migrate => {
                        awaken_credential_store::postgres::connect_migrated_pair(url)
                            .await
                            .map_err(|error| format!("connect credential Postgres: {error}"))?
                    }
                    PostgresSchemaMode::Verify => {
                        awaken_credential_store::postgres::connect_existing_pair(url)
                            .await
                            .map_err(|error| format!("connect credential Postgres: {error}"))?
                    }
                };
                (
                    Arc::new(creds),
                    Arc::new(awaken_credential_store::SealedAeadSecretStore::over(
                        key,
                        Arc::new(blobs),
                    )),
                )
            }
        };
        (Some(triple.0), Some(triple.1))
    } else {
        (None, None)
    };

    // The admin aggregate backs three Control ports (profiles / Agent resource
    // bindings / webhooks) off one store.
    let mut admin_profiles: Option<Arc<dyn awaken_config_resolver::InferenceProfileStore>> = None;
    let mut admin_webhooks: Option<Arc<dyn awaken_config_resolver::WebhookStore>> = None;
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

    let environment_work: Option<Arc<dyn awaken_session_contract::work_queue::WorkQueue>> =
        if role_owns_managed_execution(role) {
            ensure_parent(&coordinator_cfg.sessions)?;
            Some(match &coordinator_cfg.sessions {
                StoreBackend::Sqlite(path_value) => Arc::new(
                    awaken_work_store::SqliteWorkQueue::open(&path(path_value))
                        .map_err(|error| format!("open work queue SQLite: {error}"))?,
                )
                    as Arc<dyn awaken_session_contract::work_queue::WorkQueue>,
                StoreBackend::Postgres(url) => Arc::new(
                    match postgres_schema {
                        PostgresSchemaMode::Migrate => {
                            awaken_work_store::PostgresWorkQueue::connect(url).await
                        }
                        PostgresSchemaMode::Verify => {
                            awaken_work_store::PostgresWorkQueue::connect_existing(url).await
                        }
                    }
                    .map_err(|error| format!("connect work queue Postgres: {error}"))?,
                )
                    as Arc<dyn awaken_session_contract::work_queue::WorkQueue>,
            })
        } else {
            None
        };

    let coordinator = if role_owns_managed_execution(role) {
        ensure_parent(&coordinator_cfg.sessions)?;
        ensure_parent(&coordinator_cfg.captured_content)?;
        let sessions: Arc<dyn awaken_session_contract::ManagedSessionRepository>;
        let deployments: Arc<dyn awaken_deployment_contract::DeploymentRepository>;
        let memory_extractions: Arc<dyn awaken_ext_memory::MemoryExtractionRepository>;
        let dream_process_store: Arc<dyn awaken_session_contract::DreamProcessStore>;
        match &coordinator_cfg.sessions {
            StoreBackend::Sqlite(p) => {
                let repository = Arc::new(
                    awaken_session_store::SqliteManagedSessionRepository::open(&path(p)).map_err(
                        |error| format!("open sessions SQLite {}: {error}", p.display()),
                    )?,
                );
                sessions = repository.clone();
                deployments = repository.clone();
                memory_extractions = repository.clone();
                dream_process_store = repository;
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
                dream_process_store = repository;
            }
        }
        let (capture_sink, captured_content_eraser): (
            Arc<dyn awaken_runtime_contract::CaptureSink>,
            Arc<dyn awaken_runtime_contract::ContentEraser>,
        ) =
            match &coordinator_cfg.captured_content {
                StoreBackend::Sqlite(p) => {
                    let store = Arc::new(
                        awaken_captured_content_store::SqliteCapturedContentStore::open(&path(p))
                            .map_err(|error| {
                            format!("open captured-content SQLite {}: {error}", p.display())
                        })?,
                    );
                    (store.clone(), store)
                }
                StoreBackend::Postgres(url) => {
                    let store = Arc::new(match postgres_schema {
                    PostgresSchemaMode::Migrate => {
                        awaken_captured_content_store::PgCapturedContentStore::connect(url).await
                    }
                    PostgresSchemaMode::Verify => {
                        awaken_captured_content_store::PgCapturedContentStore::connect_existing(url)
                            .await
                    }
                }
                .map_err(|error| format!("connect captured-content Postgres: {error}"))?);
                    (store.clone(), store)
                }
            };
        Some(CoordinatorStores {
            resources: resources
                .ok_or_else(|| "Coordinator stores require Resources application".to_owned())?,
            sessions,
            deployments,
            memory_extractions,
            dream_process_store,
            capture_sink,
            captured_content_eraser,
            environment_work: environment_work.expect("Coordinator role opens WorkQueue"),
        })
    } else {
        None
    };

    let config: Option<Arc<dyn awaken_agent_config::ScopedConfigRegistry>> = if opens_control {
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

    let data_subject = if opens_control {
        ensure_parent(&cfg.data_subject)?;
        let repo: Arc<dyn awaken_data_subject_application::DataSubjectRepo>;
        let jobs: Arc<dyn awaken_data_subject_application::ErasureJobRepo>;
        match &cfg.data_subject {
            StoreBackend::Sqlite(p) => {
                let store = Arc::new(
                    awaken_data_subject_store::SqliteDataSubjectRepo::open(&path(p)).map_err(
                        |error| format!("open data-subject SQLite {}: {error}", p.display()),
                    )?,
                );
                repo = store.clone();
                jobs = store;
            }
            StoreBackend::Postgres(url) => {
                let store = Arc::new(
                    match postgres_schema {
                        PostgresSchemaMode::Migrate => {
                            awaken_data_subject_store::PgDataSubjectRepo::connect(url).await
                        }
                        PostgresSchemaMode::Verify => {
                            awaken_data_subject_store::PgDataSubjectRepo::connect_existing(url)
                                .await
                        }
                    }
                    .map_err(|error| format!("connect data-subject Postgres: {error}"))?,
                );
                repo = store.clone();
                jobs = store;
            }
        }
        Some((repo, jobs))
    } else {
        None
    };

    let control_environment = if opens_control {
        ensure_parent(&cfg.environment)?;
        Some(match &cfg.environment {
            StoreBackend::Sqlite(environment_path) => (
                Arc::new(
                    awaken_env_store::SqliteEnvRegistry::open(&path(environment_path))
                        .map_err(|error| format!("open environments SQLite: {error}"))?,
                ) as Arc<dyn awaken_environment_contract::EnvRegistry>,
                Arc::new(
                    awaken_sandbox_policy_store::SqliteSandboxExecutionPolicyStore::open(&path(
                        environment_path,
                    ))
                    .map_err(|error| format!("open sandbox policies SQLite: {error}"))?,
                )
                    as Arc<dyn awaken_provisioning_contract::SandboxExecutionPolicyStore>,
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
                let sandbox_policies = match postgres_schema {
                    PostgresSchemaMode::Migrate => {
                        awaken_sandbox_policy_store::PostgresSandboxExecutionPolicyStore::connect(
                            url,
                        )
                        .await
                    }
                    PostgresSchemaMode::Verify => {
                        awaken_sandbox_policy_store::PostgresSandboxExecutionPolicyStore::connect_existing(url).await
                    }
                }
                .map_err(|error| format!("connect sandbox policies Postgres: {error}"))?;
                (
                    Arc::new(environments) as Arc<dyn awaken_environment_contract::EnvRegistry>,
                    Arc::new(sandbox_policies)
                        as Arc<dyn awaken_provisioning_contract::SandboxExecutionPolicyStore>,
                )
            }
        })
    } else {
        None
    };

    Ok(ProcessStores {
        workspace_root: Some(workspace_root),
        control: if opens_control {
            let (environments, sandbox_policies) =
                control_environment.expect("Control role opens Environment stores");
            Some(ControlStores {
                catalog: catalog.expect("Control role opens Catalog"),
                credentials: credentials.expect("Control role opens CredentialRepo"),
                secrets: secrets.expect("Control role opens SecretStore"),
                profiles: admin_profiles.expect("Control role opens profile store"),
                resources: admin_resources.expect("Control role opens Resource authoring store"),
                webhooks: admin_webhooks.expect("Control role opens Webhook store"),
                config: config.expect("Control role opens Config store"),
                data_subjects: data_subject
                    .as_ref()
                    .expect("Control role opens Data Subject store")
                    .0
                    .clone(),
                erasure_jobs: data_subject
                    .expect("Control role opens Data Subject store")
                    .1,
                environments,
                sandbox_policies,
            })
        } else {
            None
        },
        coordinator,
    })
}

/// Build all-in-one from the standard typed deployment configuration.
pub async fn build_all_in_one_router() -> Router {
    build_all_in_one_router_with_model_supply(
        PublicationModelSupply::PublishedProviders,
        ManagedServiceAdapters::default(),
    )
    .await
}

/// Hermetic all-in-one startup for tests and embedders that explicitly want
/// volatile stores. It never consults the standard deployment config path.
#[cfg(any(test, feature = "test-support"))]
pub async fn build_ephemeral_all_in_one_router() -> Router {
    let stores = in_memory_process_stores();
    let options = exact_host_model::local_test_process_options(&stores);
    prepare_runtime_routers(
        stores,
        None,
        None,
        None,
        PublicationModelSupply::PublishedProviders,
        options,
        None,
    )
    .await
    .expect("prepare ephemeral all-in-one process")
    .public_router
}

/// Canonical product process from the command's one resolved configuration.
pub async fn build_all_in_one_router_with_deployment(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
) -> Result<Router, String> {
    prepare_all_in_one_process(deployment, key)
        .await
        .map(|process| process.public_router)
}

pub async fn prepare_all_in_one_process(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
) -> Result<PreparedProcess, String> {
    prepare_runtime_process(
        deployment,
        Some(key),
        config::Role::AllInOne,
        PublicationModelSupply::PublishedProviders,
        ManagedServiceAdapters::default(),
    )
    .await
}

/// Prepare the canonical AllInOne process with product-supplied managed and
/// Coordinator infrastructure adapters.
///
/// Commercial compositions use this seam to retain Awaken's complete
/// Managed Agents and Environment surface while replacing only narrow
/// infrastructure ports. Routers, stores, migrations, and lifecycle remain
/// owned by Awaken.
pub async fn prepare_all_in_one_process_with_services(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
    managed_services: ManagedServiceAdapters,
    coordinator_services: CoordinatorServiceAdapters,
) -> Result<PreparedProcess, String> {
    prepare_runtime_process_with_coordinator_services(
        deployment,
        Some(key),
        config::Role::AllInOne,
        PublicationModelSupply::PublishedProviders,
        managed_services,
        coordinator_services,
    )
    .await
}

/// Coordinator-only process. It reuses the exact same Session, Run,
/// Dispatch, and Worker-coordination construction as all-in-one, while the
/// shared role selector keeps Control routes outside the exposed API.
pub async fn prepare_coordinator_process(
    deployment: &config::ResolvedDeployment,
) -> Result<PreparedProcess, String> {
    prepare_runtime_process(
        deployment,
        None,
        config::Role::Coordinator,
        PublicationModelSupply::PublishedProviders,
        ManagedServiceAdapters::default(),
    )
    .await
}

/// Prepare the one canonical Coordinator with hosted infrastructure adapters.
/// The returned routers are still built exclusively by
/// `awaken-coordinator`; callers may merge only non-overlapping product routes.
pub async fn prepare_coordinator_process_with_services(
    deployment: &config::ResolvedDeployment,
    managed_services: ManagedServiceAdapters,
    coordinator_services: CoordinatorServiceAdapters,
) -> Result<PreparedProcess, String> {
    prepare_runtime_process_with_coordinator_services(
        deployment,
        None,
        config::Role::Coordinator,
        PublicationModelSupply::PublishedProviders,
        managed_services,
        coordinator_services,
    )
    .await
}

/// Build the real env-selected management surface with one explicit in-process
/// scenario executor. This is a dev/e2e startup, not a provider fallback:
/// its exact host candidate is published by a dedicated resolver and no
/// credential/provider materializer is installed.
#[cfg(any(test, feature = "test-support"))]
pub async fn build_all_in_one_router_with_scenario_model(
    model: Arc<dyn LlmExecutor>,
    model_ref: String,
    managed_services: ManagedServiceAdapters,
) -> Router {
    build_all_in_one_router_with_model_supply(
        PublicationModelSupply::Host {
            executor: model,
            binding: awaken_runtime_contract::resolved::ModelBinding::new(
                "default", model_ref, "default",
            ),
        },
        managed_services,
    )
    .await
}

async fn build_all_in_one_router_with_model_supply(
    model_supply: PublicationModelSupply,
    managed_services: ManagedServiceAdapters,
) -> Router {
    let deployment = config::ResolvedDeployment::load(config::ConfigOverrides::default())
        .unwrap_or_else(|error| panic!("deployment configuration: {error}"));
    let key = deployment
        .seal_key
        .load_or_create()
        .unwrap_or_else(|error| panic!("control seal key: {error}"));
    prepare_runtime_process(
        &deployment,
        Some(&key),
        config::Role::AllInOne,
        model_supply,
        managed_services,
    )
    .await
    .unwrap_or_else(|error| panic!("prepare all-in-one process: {error}"))
    .public_router
}

/// [`build_all_in_one_router_with_model`] plus explicit test-only process inputs.
/// `web_search_providers` is selected before Control and Host are assembled, so
/// publication, capability discovery, and dispatch all receive the same registry.
/// `customize_host` remains the last-mile seam for a runtime backend the standard
/// product process does not provide, e.g. `host.with_acp(executor)` so `acp:*`
/// threads run on an external CLI while the full managed plane (vault + MCP staging +
/// config plane) is still in play. Keeps the ACP executor's crate out of this module.
#[cfg(any(test, feature = "test-support"))]
pub async fn build_all_in_one_router_with_host_customizer(
    model: Arc<dyn LlmExecutor>,
    binding: awaken_runtime_contract::resolved::ModelBinding,
    web_search_providers: Option<awaken_ext_builtin_tools::WebSearchProviderRegistry>,
    customize_host: impl FnOnce(SharedHost) -> SharedHost + Send + 'static,
) -> Router {
    let stores = in_memory_process_stores();
    let mut options = exact_host_model::local_test_process_options(&stores);
    options.web_search_providers = web_search_providers;
    prepare_runtime_routers(
        stores,
        None,
        None,
        None,
        PublicationModelSupply::Host {
            executor: model,
            binding,
        },
        options,
        Some(Box::new(customize_host)),
    )
    .await
    .expect("prepare test-support customized all-in-one process")
    .public_router
}

/// Durable counterpart of [`build_all_in_one_router_with_host_customizer`].
///
/// This is an explicit-input startup seam for restart tests and embeddings
/// that need a real external runtime while retaining the same management and
/// resource-plane state across host lifetimes. The sealing key and storage root
/// are supplied by the caller, avoiding process-global environment races.
#[cfg(any(test, feature = "test-support"))]
pub async fn build_durable_all_in_one_router_with_host_customizer(
    dir: &std::path::Path,
    key: &[u8; 32],
    model: Arc<dyn LlmExecutor>,
    binding: awaken_runtime_contract::resolved::ModelBinding,
    customize_host: impl FnOnce(SharedHost) -> SharedHost + Send + 'static,
) -> Router {
    let stores = open_local_process_stores(dir, key)
        .await
        .unwrap_or_else(|error| panic!("open local deployment stores: {error}"));
    let options = exact_host_model::local_test_process_options(&stores);
    prepare_runtime_routers(
        stores,
        None,
        None,
        None,
        PublicationModelSupply::Host {
            executor: model,
            binding,
        },
        options,
        Some(Box::new(customize_host)),
    )
    .await
    .expect("prepare durable customized all-in-one process")
    .public_router
}

/// Build the management router over in-memory stores with an explicit host default
/// model injected — a **test-only** seam so an integration test can drive the real
/// management router with a deterministic (mock) model, keeping the mock out of the
/// production process.
#[cfg(any(test, feature = "test-support"))]
pub async fn build_all_in_one_router_with_model(
    model: Arc<dyn LlmExecutor>,
    model_ref: impl Into<String>,
) -> Router {
    let stores = in_memory_process_stores();
    let options = exact_host_model::local_test_process_options(&stores);
    prepare_runtime_routers(
        stores,
        None,
        None,
        None,
        PublicationModelSupply::Host {
            executor: model,
            binding: awaken_runtime_contract::resolved::ModelBinding::new(
                "default", model_ref, "genai",
            ),
        },
        options,
        None,
    )
    .await
    .expect("prepare test-support modeled all-in-one process")
    .public_router
}

/// [`build_all_in_one_router`] with explicit persistence inputs (no environment
/// read): durable all-in-one over `dir`, sealing secrets under `key`.
/// Exposed so a restart test can rebuild a router over one directory across
/// simulated process lifetimes without racing on process-global env vars.
/// No IAM guard; available only to explicit test-support deployments.
#[cfg(any(test, feature = "test-support"))]
pub async fn build_durable_all_in_one_router(dir: &std::path::Path, key: &[u8; 32]) -> Router {
    let stores = open_local_process_stores(dir, key)
        .await
        .unwrap_or_else(|error| panic!("open local deployment stores: {error}"));
    let options = exact_host_model::local_test_process_options(&stores);
    prepare_runtime_routers(
        stores,
        None,
        None,
        None,
        PublicationModelSupply::PublishedProviders,
        options,
        None,
    )
    .await
    .expect("prepare durable all-in-one process")
    .public_router
}

/// [`build_durable_all_in_one_router`] with the embedded IAM guard enabled — the
/// typed self-managed identity startup. Returns the
/// [`ManagementAuthz`] handle too so a test (or an embedding) can mint further
/// workspace tokens against the same policy state.
#[cfg(any(test, feature = "test-support"))]
pub async fn build_secured_all_in_one_router(
    dir: &std::path::Path,
    key: &[u8; 32],
) -> (Router, Arc<ManagementAuthz>) {
    let iam = embedded_iam(dir);
    let stores = open_local_process_stores(dir, key)
        .await
        .unwrap_or_else(|error| panic!("open local deployment stores: {error}"));
    let options = exact_host_model::local_test_process_options(&stores);
    let router = prepare_runtime_routers(
        stores,
        Some(iam.clone()),
        None,
        None,
        PublicationModelSupply::PublishedProviders,
        options,
        None,
    )
    .await
    .expect("prepare secured all-in-one process")
    .public_router;
    (router, iam)
}

fn brokered_inference_client(
    cloud_models_enabled: bool,
    remote_iam: Option<&Arc<RemoteManagementAuthz>>,
    cloud_api_base_url: Option<&str>,
    execution_workspace: &str,
) -> Option<Arc<awaken_credential_materializer::brokered_inference::HttpBrokeredInferenceClient>> {
    if !cloud_models_enabled {
        return None;
    }
    let authz = Arc::clone(remote_iam?);
    let token_source: Arc<awaken_agent_contract::RedactedStringSource> = Arc::new(move || {
        authz
            .cloud_user_token()?
            .ok_or_else(|| "Cloud login credential is unavailable".to_string())
    });
    let base_url =
        cloud_api_base_url.expect("Awaken Cloud identity requires a Cloud inference API URL");
    Some(Arc::new(
        awaken_credential_materializer::brokered_inference::HttpBrokeredInferenceClient::new(
            base_url,
            token_source,
            execution_workspace,
        )
        .unwrap_or_else(|error| panic!("Cloud inference configuration: {error}")),
    ))
}

fn resolve_model_services(
    supply: PublicationModelSupply,
    stores: &ProcessStores,
    cloud_models_enabled: bool,
    worker_observations: Arc<dyn awaken_coordinator::WorkerObservationSource>,
) -> ResolvedModelServices {
    let control = stores
        .control
        .as_ref()
        .expect("model publication requires Control stores");
    let acp_capabilities = Arc::new(awaken_run_executor_acp::known_acp_publication_capabilities());
    match supply {
        PublicationModelSupply::PublishedProviders => ResolvedModelServices {
            publication_resolver: Arc::new(
                awaken_control::model_publication::CatalogModelPublicationResolver::from_repo(
                    control.catalog.clone(),
                    control.credentials.clone(),
                )
                .with_acp_capabilities(acp_capabilities)
                .with_profiles(control.profiles.clone())
                .with_worker_observations(worker_observations)
                .with_brokered_access(cloud_models_enabled),
            ),
            runtime: RuntimeModelServices::PublishedProviders,
        },
        PublicationModelSupply::HostedPublication { resolver } => ResolvedModelServices {
            publication_resolver: resolver,
            runtime: RuntimeModelServices::NoModelConfigured,
        },
        #[cfg(any(test, feature = "test-support"))]
        PublicationModelSupply::Host { executor, binding } => {
            let model_ref = binding.model_ref.clone();
            ResolvedModelServices {
                publication_resolver: Arc::new(ExactHostModelPublicationResolver { binding }),
                runtime: RuntimeModelServices::Host {
                    executor,
                    model_ref,
                },
            }
        }
    }
}

fn runtime_model_wiring(
    runtime: RuntimeModelServices,
    credential_materializer: &awaken_credential_materializer::PinnedCredentialMaterializer,
    cloud_models_enabled: bool,
    brokered_client: Option<
        &Arc<awaken_credential_materializer::brokered_inference::HttpBrokeredInferenceClient>,
    >,
) -> RuntimeModelWiring {
    match runtime {
        RuntimeModelServices::PublishedProviders => RuntimeModelWiring {
            executor: Arc::new(awaken_runtime_host::NoModelConfiguredExecutor),
            model_ref: awaken_runtime_host::UNCONFIGURED_MODEL_REF.to_string(),
            materializer: Some(Arc::new({
                let materializer =
                    awaken_credential_materializer::CredentialInferenceMaterializer::from_pinned(
                        credential_materializer.clone(),
                    )
                    .with_brokered_mode(cloud_models_enabled);
                match brokered_client {
                    Some(client) => materializer.with_brokered_client(client.clone()),
                    None => materializer,
                }
            })),
        },
        RuntimeModelServices::NoModelConfigured => RuntimeModelWiring {
            executor: Arc::new(awaken_runtime_host::NoModelConfiguredExecutor),
            model_ref: awaken_runtime_host::UNCONFIGURED_MODEL_REF.to_string(),
            materializer: None,
        },
        #[cfg(any(test, feature = "test-support"))]
        RuntimeModelServices::Host {
            executor,
            model_ref,
        } => RuntimeModelWiring {
            executor,
            model_ref,
            materializer: None,
        },
    }
}

/// Prepare Coordinator and, for AllInOne, include the canonical Control service.
/// The data plane comes from [`awaken_coordinator::mount_with_managed`]; this
/// process boundary joins their HTTP surfaces and supervises lifecycle without
/// rebuilding either domain application.
/// Resolve the hidden local Org from one process-startup seam. Self-managed
/// deployments may explicitly configure it; single-machine mode never asks the
/// user and consistently uses the default Org.
fn local_org_id() -> String {
    awaken_control::DEFAULT_ORG_ID.to_owned()
}

// Seal-key parsing and its decision tests live with the authoritative AEAD
// adapter in `awaken_credential_store::sealed`.

#[cfg(test)]
mod runtime_session_store_tests {
    use super::*;
    use awaken_config_service::ModelPublicationResolver;
    use awaken_session_contract::{
        ControlSessionCreationInputs, EnvironmentFingerprint, EnvironmentRevision,
        EnvironmentSnapshot, IdempotencyRecord, PersistedSession, SessionBaselineState,
        SessionCreationIntent, SessionNetworkPolicy, stable_fingerprint,
    };

    fn creation_intent() -> SessionCreationIntent {
        SessionCreationIntent {
            control: ControlSessionCreationInputs {
                environment: EnvironmentSnapshot {
                    environment_id: awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID
                        .into(),
                    revision: EnvironmentRevision(1),
                    self_hosted: false,
                    config_fingerprint: EnvironmentFingerprint("env-local".into()),
                    sandbox: Default::default(),
                    sandbox_provisioning: Default::default(),
                    idle_retention: Default::default(),
                    packages: Default::default(),
                    prepared_image: None,
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
                runtime_placement: awaken_session_contract::SessionRuntimePlacement::Local,
                agent_id: "assistant".into(),
                agent_revision: None,
                model_override: None,
                model: "test-model".into(),
                execution_model_ref: "test-model".into(),
                runtime: None,
                mcp_authoring: Default::default(),
                delegate_ids: Vec::new(),
                toolsets: Vec::new(),
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: Vec::new(),
                transcript_prefix: None,
                resources: Default::default(),
                initial_mcp: Vec::new(),
            },
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
            budget: Default::default(),
            event_batches: Vec::new(),
            activity_epoch: 0,
            active_activity_epochs: Default::default(),
            running_interval: None,
            runtime_active_millis: 0,
            environment: Default::default(),
            mcp: Default::default(),
            resources: Default::default(),
            realization: None,
            realization_progress: Default::default(),
            execution: awaken_session_contract::SessionExecutionState::Idle,
            disposition: Default::default(),
            terminal_cleanup: Default::default(),
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
                .expect("test startup owns Managed Execution");
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
            .expect("test startup owns Managed Execution");
        assert_eq!(
            execution.sessions.owner("sesn-restart").await.as_deref(),
            Ok("workspace-a")
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
        // D2 ephemeral startup -> the same single-allocation invariant holds.
        // A different address would expose a parallel Deployment truth before
        // the repository-backed Deployment component is prepared.
        let dir = tempfile::tempdir().expect("temporary runtime storage");
        let durable = process_stores_for_runtime_storage(Some(dir.path()));
        let durable = durable
            .coordinator
            .as_ref()
            .expect("durable test startup owns Managed Execution");
        assert_eq!(
            Arc::as_ptr(&durable.sessions) as *const (),
            Arc::as_ptr(&durable.deployments) as *const (),
            "D1"
        );

        let ephemeral = process_stores_for_runtime_storage(None);
        let ephemeral = ephemeral
            .coordinator
            .as_ref()
            .expect("ephemeral test startup owns Managed Execution");
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
                &awaken_agent_config::ModelSelection::Pinned(
                    awaken_runtime_contract::resolved::ModelBinding::new("", "scenario", ""),
                ),
                &[],
            )
            .await
            .expect("model-only SDK/UI selection resolves through the host adapter");

        assert_eq!(resolved.primary.binding(), &binding);
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
                &awaken_agent_config::ModelSelection::Pinned(
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

    fn executable_agent_registration_body() -> Vec<u8> {
        let mut snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent-a")
            .model(awaken_runtime_contract::resolved::ModelBinding::new(
                "test", "model", "native",
            ))
            .fingerprint("fp-a")
            .build();
        snapshot.metadata = awaken_runtime_contract::AgentSnapshotMetadata {
            source: awaken_runtime_contract::AgentConfigRevisionRef {
                agent_id: awaken_runtime_contract::snapshot::AgentId("agent-a".into()),
                revision: 1,
            },
            publication_version: awaken_runtime_contract::AgentPublicationVersion("v1".into()),
            resolution: Default::default(),
            fingerprint: awaken_runtime_contract::AgentSnapshotFingerprint("fp-a".into()),
        };
        serde_json::to_vec(
            &awaken_executable_agent_contract::ExecutableAgentRegistration {
                workspace_id: "workspace-a".into(),
                agent_id: "agent-a".into(),
                source_revision: 1,
                snapshot,
                session_profile: Default::default(),
            },
        )
        .unwrap()
    }

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
        // AllInOne configures it locally, so both open the Session/Deployment store
        // group. R3 Control and R4 Worker must never create a second local group.
        assert!(role_owns_managed_execution(config::Role::Coordinator), "R1");
        assert!(role_owns_managed_execution(config::Role::AllInOne), "R2");
        assert!(!role_owns_managed_execution(config::Role::Control), "R3");
        assert!(!role_owns_managed_execution(config::Role::Worker), "R4");
    }

    #[test]
    fn resource_authorities_have_only_resource_serving_process_owners() {
        // Cause/effect decision table:
        // R1 AllInOne and R2 Coordinator serve claim-fenced Resource APIs and
        // therefore host the canonical authorities. R3 Control consumes only
        // Resource authoring/read services; R4 Worker consumes authenticated per-kind
        // clients, so neither may open Resource authorities.
        assert!(role_hosts_resources(config::Role::AllInOne), "R1");
        assert!(role_hosts_resources(config::Role::Coordinator), "R2");
        assert!(!role_hosts_resources(config::Role::Control), "R3");
        assert!(!role_hosts_resources(config::Role::Worker), "R4");
    }

    /// Cause/effect decision table:
    ///
    /// | role/listener | authoring API | Session/Deployment | Worker transport | service API |
    /// | --- | --- | --- | --- | --- |
    /// | Control/public | mounted | absent | absent | absent |
    /// | Control/private | absent | absent | absent | authenticated |
    /// | Coordinator/public | absent | mounted | absent | absent |
    /// | Coordinator/private | absent | absent | authenticated once | authenticated |
    ///
    /// AllInOne local-adapter startup is covered by the existing full-surface integration
    /// suites; this test owns the two exclusion rules that those suites cannot
    /// prove. Moving the startup body into `runtime_process_router` adds no
    /// new condition or outcome, so a separate decision table is inapplicable:
    /// these same route-presence/absence effects are the structural-extraction
    /// regression coverage. The hosted custom-publication entry point delegates
    /// to this same process and only substitutes publication SPIs, so these
    /// listener-presence and listener-absence rules cover that projection too.
    /// The Worker column includes dispatch, resources, commit, and Environment
    /// warmup. Building the split Coordinator proves the common returned Worker
    /// router is merged once: a second warmup route merge would make Axum reject
    /// the duplicate route during construction rather than reach these assertions.
    #[tokio::test]
    async fn service_roles_expose_only_their_owned_api() {
        let control_routers = prepare_control_routers(
            in_memory_control_stores(),
            None,
            None,
            None,
            PublicationModelSupply::PublishedProviders,
            ProcessStartup {
                role: config::Role::Control,
                executable_environment_wiring: Some(
                    executable_environment_registration::local_test_wiring(),
                ),
                executable_agent_wiring: Some(
                    executable_agent_registration::ExecutableAgentWiring::local(),
                ),
                worker_observations: Some(
                    worker_observation_wiring::WorkerObservationWiring::local(
                        awaken_coordinator::test_worker_directory(),
                    ),
                ),
                control_service_authenticator: Some(
                    awaken_service_auth_contract::static_token_authenticator("control-token")
                        .unwrap(),
                ),
                ..Default::default()
            },
        )
        .await
        .expect("Control MCP exports have matching descriptors and executors");
        let app = control_routers.public_router;

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

        for path in [
            awaken_executable_environment_catalog::EXECUTABLE_ENVIRONMENT_REGISTER_PATH,
            awaken_coordinator::worker_observation_boundary::WORKER_OBSERVATIONS_PATH,
            awaken_coordinator::data_subject_boundary::ERASE_COORDINATOR_CONTENT_PATH,
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::post(path)
                        .header("content-type", "application/json")
                        .header("authorization", "Bearer registration-token")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }

        let internal_control = app
            .clone()
            .oneshot(
                Request::post("/internal/v1/control/audit/get")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer control-token")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(internal_control.status(), StatusCode::NOT_FOUND);

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

        let private_control = control_routers.private_router;
        let public_on_private = private_control
            .clone()
            .oneshot(
                Request::get("/v1/config/catalog")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(public_on_private.status(), StatusCode::NOT_FOUND);
        let unauthorized = private_control
            .clone()
            .oneshot(
                Request::post("/internal/v1/control/audit/get")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        let authenticated = private_control
            .oneshot(
                Request::post("/internal/v1/control/audit/get")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer control-token")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(authenticated.status(), StatusCode::UNAUTHORIZED);
        assert_ne!(authenticated.status(), StatusCode::NOT_FOUND);

        let (coordinator_stores, control_service) = in_memory_split_coordinator();
        let coordinator_data = tempfile::tempdir().expect("coordinator test data");
        let mut coordinator_deployment =
            config::local_test_deployment(coordinator_data.path().to_owned());
        // Session execution ownership decision table:
        // C1=split Coordinator, C2=local pool disabled, C3=the configured sandbox
        // cannot be realized (K8s without an image). R1(C1+C2+C3)->startup serves
        // Coordinator APIs without probing/constructing a local sandbox; registered
        // Workers remain the only Session physical-effect owners.
        coordinator_deployment.runtime.disable_local_pool = true;
        coordinator_deployment.runtime.sandbox_tier = awaken_runtime_host::SandboxTier::K8s;
        coordinator_deployment.runtime.container_image = None;
        let executable_environment_wiring =
            executable_environment_registration::ExecutableEnvironmentWiring::local(
                coordinator_stores
                    .coordinator
                    .as_ref()
                    .expect("Coordinator test owns Environment work")
                    .environment_work
                    .clone(),
            )
            .expect("configure Coordinator test Environment wiring");
        let worker_directory = awaken_coordinator::test_worker_directory();
        let coordinator_routers = prepare_runtime_routers(
            coordinator_stores,
            None,
            None,
            None,
            PublicationModelSupply::PublishedProviders,
            ProcessStartup {
                role: config::Role::Coordinator,
                executable_agent_wiring: Some(
                    executable_agent_registration::ExecutableAgentWiring::local_server(
                        "registration-token",
                    ),
                ),
                executable_environment_wiring: Some(executable_environment_wiring),
                control_service: Some(control_service),
                deployment: Some(coordinator_deployment.runtime),
                worker_directory: Some(worker_directory.clone()),
                runtime_authority: None,
                worker_observations: Some(
                    worker_observation_wiring::WorkerObservationWiring::local(worker_directory),
                ),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("prepare split Coordinator test process");
        let app = coordinator_routers.public_router;
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
        assert_eq!(registration.status(), StatusCode::NOT_FOUND);

        for path in [
            awaken_executable_environment_catalog::EXECUTABLE_ENVIRONMENT_REGISTER_PATH,
            awaken_coordinator::worker_observation_boundary::WORKER_OBSERVATIONS_PATH,
            awaken_coordinator::data_subject_boundary::ERASE_COORDINATOR_CONTENT_PATH,
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::post(path)
                        .header("content-type", "application/json")
                        .header("authorization", "Bearer registration-token")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }

        let deployments = app
            .clone()
            .oneshot(Request::get("/v1/deployments").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(deployments.status(), StatusCode::OK);

        let worker_paths = [
            "/v1/worker/dispatch/claim",
            "/v1/worker/environment/warmups",
            "/v1/worker/resources/files/content",
            "/v1/worker/commit-claimed",
        ];
        for path in worker_paths {
            let response = app
                .clone()
                .oneshot(
                    Request::post(path)
                        .header("content-type", "application/json")
                        .header(
                            awaken_worker_transport_security::WORKER_ID_HEADER,
                            "worker-test",
                        )
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "public {path}");
        }

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

        let private_coordinator = coordinator_routers.private_router;
        let session_on_private = private_coordinator
            .clone()
            .oneshot(Request::get("/v1/sessions").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(session_on_private.status(), StatusCode::NOT_FOUND);
        for path in worker_paths {
            let unauthorized = private_coordinator
                .clone()
                .oneshot(
                    Request::post(path)
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                unauthorized.status(),
                StatusCode::UNAUTHORIZED,
                "private unauthenticated {path}"
            );
            let authenticated = private_coordinator
                .clone()
                .oneshot(
                    Request::post(path)
                        .header("content-type", "application/json")
                        .header(
                            awaken_worker_transport_security::WORKER_ID_HEADER,
                            "worker-test",
                        )
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_ne!(
                authenticated.status(),
                StatusCode::UNAUTHORIZED,
                "private authenticated {path}"
            );
            assert_ne!(
                authenticated.status(),
                StatusCode::NOT_FOUND,
                "private mounted {path}"
            );
        }
        let unauthorized = private_coordinator
            .clone()
            .oneshot(
                Request::post(awaken_executable_agent_catalog::EXECUTABLE_AGENT_REGISTER_PATH)
                    .header("content-type", "application/json")
                    .body(Body::from(executable_agent_registration_body()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        let authenticated = private_coordinator
            .oneshot(
                Request::post(awaken_executable_agent_catalog::EXECUTABLE_AGENT_REGISTER_PATH)
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer registration-token")
                    .body(Body::from(executable_agent_registration_body()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authenticated.status(), StatusCode::OK);
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
            _selection: &awaken_agent_config::ModelSelection,
            _candidates: &[awaken_runtime_contract::resolved::ModelBinding],
        ) -> Result<
            awaken_config_service::ResolvedPublicationModels,
            awaken_config_service::PublicationResolutionError,
        > {
            self.called.store(true, Ordering::SeqCst);
            Err(awaken_config_service::PublicationResolutionError::MissingPrimary)
        }
    }

    #[test]
    fn hosted_publication_does_not_require_an_interactive_broker_client() {
        // Cause/effect decision table: C1=published-provider composition,
        // C2=hosted resolver composition. R1(C1)->interactive broker fallback;
        // R2(C2)->no fallback because the host-supplied resolver/catalog are
        // authoritative and a projected workload token is not a user token.
        assert!(
            PublicationModelSupply::PublishedProviders.needs_interactive_brokered_client(),
            "R1"
        );
        assert!(
            !PublicationModelSupply::HostedPublication {
                resolver: Arc::new(RecordingHostedResolver {
                    called: Arc::new(AtomicBool::new(false)),
                }),
            }
            .needs_interactive_brokered_client(),
            "R2"
        );
    }

    /// Causal graph:
    /// hosted resolver injection -> canonical ConfigService publication
    /// -> the injected resolver is the only model-selection authority
    /// -> its fail-closed result is returned without consulting the local catalog.
    ///
    /// Decision table:
    /// | control startup | resolver result | local catalog | outcome |
    /// | --- | --- | --- | --- |
    /// | default open | any | authoritative | catalog resolver decides |
    /// | hosted injection | success | irrelevant | injected candidate publishes |
    /// | hosted injection | failure | populated/empty | publication fails closed |
    #[tokio::test]
    async fn hosted_control_uses_the_injected_publication_resolver() {
        let called = Arc::new(AtomicBool::new(false));
        let app = prepare_control_routers(
            in_memory_control_stores(),
            None,
            None,
            None,
            PublicationModelSupply::HostedPublication {
                resolver: Arc::new(RecordingHostedResolver {
                    called: called.clone(),
                }),
            },
            ProcessStartup {
                role: config::Role::Control,
                executable_environment_wiring: Some(
                    executable_environment_registration::local_test_wiring(),
                ),
                executable_agent_wiring: Some(
                    executable_agent_registration::ExecutableAgentWiring::local(),
                ),
                worker_observations: Some(
                    worker_observation_wiring::WorkerObservationWiring::local(
                        awaken_coordinator::test_worker_directory(),
                    ),
                ),
                ..Default::default()
            },
        )
        .await
        .expect("Control MCP exports have matching descriptors and executors")
        .public_router;

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
