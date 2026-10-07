// @formal entrypoint assembly_role.process_role_projects_exact_startup_authority
// @formal entrypoint assembly_role.worker_never_acquires_service_startup_or_migration_authority
// @formal entrypoint service_lifecycle.startup_wiring_fails_closed_for_unknown_missing_or_extra_authority
// @formal entrypoint service_lifecycle.startup_wiring_is_exact_for_every_service_role
// @formal entrypoint service_lifecycle.startup_wiring_requires_every_role_owned_component
//! Role-owned store groups used by product-process startup.
//!
//! These types make acquisition authority explicit: a split process receives
//! either Control or Coordinator stores, while AllInOne receives both canonical
//! groups.

use std::sync::Arc;

use awaken_service_lifecycle::StartupComponent;

use crate::config;
use crate::postgres_pools;
use awaken_session_executor::SharedHost;

pub(super) struct ProcessStores {
    /// Durable installation root used to persist the platform Workspace id.
    pub(super) workspace_root: Option<std::path::PathBuf>,
    /// The one installation Workspace coordinate resolved by store startup and
    /// passed unchanged to IAM, Control, and Coordinator adapters.
    pub(super) platform_workspace: String,
    pub(super) control: Option<ControlStores>,
    pub(super) coordinator: Option<CoordinatorStores>,
    /// The process's one wall clock. Chosen once by the process composition,
    /// used by every store opened here and handed to every component built
    /// over these stores, so no two owners judge time on different clocks.
    pub(super) clock: Arc<dyn awaken_time::WallClock>,
}

pub(super) struct ControlStores {
    pub(super) catalog: Arc<dyn awaken_model_catalog::repo::CatalogRepo>,
    pub(super) credentials: Arc<dyn awaken_credential_vault::repo::ManagedCredentialRepository>,
    pub(super) secrets: Arc<dyn awaken_credential_vault::SecretStore>,
    pub(super) mcp_config: Arc<dyn awaken_agent_config::mcp_config::McpConfigurationRepository>,
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

/// Port views over one admin aggregate; no independently opened MCP database.
pub(super) struct AdminStoreViews {
    pub(super) mcp: Arc<dyn awaken_agent_config::mcp_config::McpConfigurationRepository>,
    pub(super) profiles: Arc<dyn awaken_config_resolver::InferenceProfileStore>,
    pub(super) resources: Arc<dyn awaken_config_resolver::AgentInputBindingRepository>,
    pub(super) webhooks: Arc<dyn awaken_config_resolver::WebhookStore>,
}
impl AdminStoreViews {
    fn from_store<T>(store: T) -> Self
    where
        T: awaken_agent_config::mcp_config::McpConfigurationRepository
            + awaken_config_resolver::InferenceProfileStore
            + awaken_config_resolver::AgentInputBindingRepository
            + awaken_config_resolver::WebhookStore
            + 'static,
    {
        let store = Arc::new(store);
        Self {
            mcp: store.clone(),
            profiles: store.clone(),
            resources: store.clone(),
            webhooks: store,
        }
    }
}
pub(super) async fn open_admin_store_views(
    backend: &awaken_control::StoreBackend,
    mode: PostgresSchemaMode,
) -> Result<AdminStoreViews, String> {
    ensure_store_parent(backend)?;
    match backend {
        awaken_control::StoreBackend::Sqlite(path) => {
            let store = awaken_admin_config_api::SqliteAdminStore::open(&path.to_string_lossy())
                .await
                .map_err(|error| format!("open admin SQLite {}: {error}", path.display()))?;
            Ok(AdminStoreViews::from_store(store))
        }
        awaken_control::StoreBackend::Postgres(url) => {
            // Schema mode remains deployment authority.
            let store = match mode {
                PostgresSchemaMode::Migrate => {
                    awaken_admin_config_api::PostgresAdminStore::connect(url).await
                }
                PostgresSchemaMode::Verify => {
                    awaken_admin_config_api::PostgresAdminStore::connect_existing(url).await
                }
            }
            .map_err(|error| format!("connect admin Postgres: {error}"))?;
            Ok(AdminStoreViews::from_store(store))
        }
    }
}

pub(super) struct CoordinatorStores {
    /// Resources is a sibling component. Coordinator mounts its ports but never
    /// opens or receives Control's Resource-authoring/admin store.
    pub(super) resources: awaken_resource_application::ResourcesApplication,
    /// Durable home for the Managed Session aggregate and lifecycle outbox.
    pub(super) sessions: Arc<dyn awaken_session_repository_contract::SessionStore>,
    /// Coordinator-owned short-lived application capabilities. It shares the
    /// selected Session database but has its own aggregate table and ledger.
    pub(super) application_access:
        Arc<awaken_coordinator::application_access_store::ApplicationAccessStore>,
    /// Coordinator-owned Deployment and DeploymentRun view over the same physical
    /// repository as Session.
    pub(super) deployments: Arc<dyn awaken_deployment_contract::DeploymentRepository>,
    /// The same physical Session store viewed through the Dream process-store port.
    pub(super) dream_process_store: Arc<dyn awaken_session_dream_contract::DreamProcessStore>,
    /// Same Session application repository viewed through the extraction-work
    /// interface; kept separate from MemoryRepository and IAM.
    pub(super) memory_extractions: Arc<dyn awaken_ext_memory::MemoryExtractionRepository>,
    /// Coordinator-owned subject-tagged content write port.
    pub(super) capture_sink: Arc<dyn awaken_runtime_contract::CaptureSink>,
    /// A second view of the exact same captured-content adapter for Control's
    /// authenticated erasure command.
    pub(super) captured_content_eraser: Arc<dyn awaken_data_governance_contract::ContentEraser>,
    pub(super) environment_work: Arc<dyn awaken_session_work_contract::WorkQueue>,
}

/// Every typed port backed by the one canonical Managed Session repository.
/// Process assembly retains these views and never opens the shared store twice.
pub(super) struct SessionStoreViews {
    pub(super) sessions: Arc<dyn awaken_session_repository_contract::SessionStore>,
    pub(super) deployments: Arc<dyn awaken_deployment_contract::DeploymentRepository>,
    pub(super) memory_extractions: Arc<dyn awaken_ext_memory::MemoryExtractionRepository>,
    pub(super) dream_process_store: Arc<dyn awaken_session_dream_contract::DreamProcessStore>,
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
    postgres_pools: &mut crate::postgres_pools::ProcessPostgresPools,
) -> Result<SessionStoreViews, String> {
    use awaken_control::StoreBackend;

    ensure_store_parent(backend)?;
    match backend {
        StoreBackend::Sqlite(path) => {
            let repository = Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open(&path.to_string_lossy())
                    .await
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
                        awaken_session_store::PostgresManagedSessionRepository::with_pool(
                            postgres_pools.get(url).await?,
                        )
                        .await
                    }
                    PostgresSchemaMode::Verify => {
                        awaken_session_store::PostgresManagedSessionRepository::with_existing_pool(
                            postgres_pools.get(url).await?,
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
    clock: Arc<dyn awaken_time::WallClock>,
    postgres_pools: &mut crate::postgres_pools::ProcessPostgresPools,
) -> Result<Arc<dyn awaken_session_work_contract::WorkQueue>, String> {
    use awaken_control::StoreBackend;

    ensure_store_parent(backend)?;
    match backend {
        StoreBackend::Sqlite(path) => Ok(Arc::new(
            awaken_work_store::SqliteWorkQueue::open(&path.to_string_lossy(), clock)
                .await
                .map_err(|error| format!("open work queue SQLite: {error}"))?,
        )),
        StoreBackend::Postgres(url) => Ok(Arc::new(
            match postgres_schema {
                PostgresSchemaMode::Migrate => {
                    awaken_work_store::PostgresWorkQueue::with_pool(
                        postgres_pools.get(url).await?,
                        clock,
                    )
                    .await
                }
                PostgresSchemaMode::Verify => {
                    awaken_work_store::PostgresWorkQueue::with_existing_pool(
                        postgres_pools.get(url).await?,
                        clock,
                    )
                    .await
                }
            }
            .map_err(|error| format!("connect work queue Postgres: {error}"))?,
        )),
    }
}

pub(super) async fn open_application_access(
    backend: &awaken_control::StoreBackend,
    postgres_schema: PostgresSchemaMode,
    clock: Arc<dyn awaken_time::WallClock>,
    postgres_pools: &mut crate::postgres_pools::ProcessPostgresPools,
) -> Result<Arc<awaken_coordinator::application_access_store::ApplicationAccessStore>, String> {
    use awaken_control::StoreBackend;

    let store = match backend {
        StoreBackend::Sqlite(path) => {
            awaken_coordinator::application_access_store::ApplicationAccessStore::open_sqlite(
                &path.to_string_lossy(),
                clock,
            )
            .await
            .map_err(|error| format!("open application access SQLite: {error}"))?
        }
        StoreBackend::Postgres(url) => match postgres_schema {
            PostgresSchemaMode::Migrate => {
                awaken_coordinator::application_access_store::ApplicationAccessStore::with_postgres_pool(
                    postgres_pools.get(url).await?, clock,
                )
                .await
            }
            PostgresSchemaMode::Verify => {
                awaken_coordinator::application_access_store::ApplicationAccessStore::with_existing_postgres_pool(
                    postgres_pools.get(url).await?, clock,
                )
                .await
            }
        }
        .map_err(|error| format!("connect application access Postgres: {error}"))?,
    };
    Ok(Arc::new(store))
}

/// Open the canonical role-owned stores selected by
/// [`ControlStoreConfig`](awaken_control::ControlStoreConfig). The compatibility
/// bundle resolves backend addresses, but this function acquires only the group
/// owned by `role`; split Control and Coordinator never share authority stores.
pub(super) struct ProcessStoreOpenOptions<'a> {
    pub(super) control: awaken_control::ControlStoreConfig,
    pub(super) coordinator: config::CoordinatorStoreConfig,
    pub(super) resources: Option<awaken_resource_application::ResourcesApplication>,
    pub(super) postgres_pools: &'a mut postgres_pools::ProcessPostgresPools,
    pub(super) workspace_root: std::path::PathBuf,
    /// Exact deployment coordinate already proven by installation admission.
    /// `None` lets the same local marker authority generate the first value.
    pub(super) platform_workspace: Option<String>,
    /// Whether this role owns persistence under `workspace_root`. Mixed
    /// local/PostgreSQL deployments publish the already-fenced remote coordinate
    /// through the same local marker after every owned store succeeds.
    pub(super) publish_local_workspace: bool,
    pub(super) seal_key: Option<&'a [u8; 32]>,
    pub(super) role: config::Role,
    pub(super) postgres_schema: PostgresSchemaMode,
    /// The process's one wall clock; see [`ProcessStores::clock`].
    pub(super) clock: Arc<dyn awaken_time::WallClock>,
}

pub(super) async fn open_process_stores(
    options: ProcessStoreOpenOptions<'_>,
) -> Result<ProcessStores, String> {
    use awaken_control::StoreBackend;

    let ProcessStoreOpenOptions {
        control: cfg,
        coordinator: coordinator_cfg,
        resources,
        postgres_pools,
        workspace_root,
        platform_workspace: remote_platform_workspace,
        publish_local_workspace,
        seal_key: key,
        role,
        postgres_schema,
        clock,
    } = options;

    let path = |p: &std::path::Path| p.to_string_lossy().into_owned();

    let opens_control = role_owns_control_component(role);
    let catalog: Option<Arc<dyn awaken_model_catalog::repo::CatalogRepo>> = if opens_control {
        ensure_store_parent(&cfg.catalog)?;
        Some(match &cfg.catalog {
            StoreBackend::Sqlite(p) => Arc::new(
                awaken_model_catalog_store::SqliteCatalogRepo::open(&path(p))
                    .await
                    .map_err(|error| format!("open catalog SQLite {}: {error}", p.display()))?,
            ),
            StoreBackend::Postgres(url) => Arc::new(
                match postgres_schema {
                    PostgresSchemaMode::Migrate => {
                        awaken_model_catalog_store::PostgresCatalogRepo::with_pool(
                            postgres_pools.get(url).await?,
                        )
                        .await
                    }
                    PostgresSchemaMode::Verify => {
                        awaken_model_catalog_store::PostgresCatalogRepo::with_existing_pool(
                            postgres_pools.get(url).await?,
                        )
                        .await
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
        ensure_store_parent(&cfg.credential)?;
        let key = key.ok_or_else(|| "Control stores require the Control seal key".to_owned())?;
        let triple: (
            Arc<dyn awaken_credential_vault::repo::ManagedCredentialRepository>,
            Arc<dyn awaken_credential_vault::SecretStore>,
        ) = match &cfg.credential {
            StoreBackend::Sqlite(p) => {
                let file = path(p);
                let (creds, blobs) = awaken_credential_store::sqlite::open_migrated_pair(&file)
                    .await
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
                        awaken_credential_store::postgres::with_pool_pair(
                            postgres_pools.get(url).await?,
                        )
                        .await
                        .map_err(|error| format!("connect credential Postgres: {error}"))?
                    }
                    PostgresSchemaMode::Verify => {
                        awaken_credential_store::postgres::with_existing_pool_pair(
                            postgres_pools.get(url).await?,
                        )
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

    let admin_views = if opens_control {
        Some(open_admin_store_views(&cfg.admin, postgres_schema).await?)
    } else {
        None
    };

    // The Managed Session repository owns the shared coordinator database's
    // canonical schema. Open it before WorkQueue or ApplicationAccess can
    // create sibling ledgers in the same file/database, then retain every port
    // view over this exact repository rather than opening it again below.
    let mut session_views = if role_owns_managed_execution(role) {
        Some(
            open_session_store_views(&coordinator_cfg.sessions, postgres_schema, postgres_pools)
                .await?,
        )
    } else {
        None
    };

    // A remote coordinate was already fenced by the PostgreSQL installation
    // gate. Local identity publication is deliberately deferred until every
    // role-owned store below has opened successfully.
    let mut platform_workspace = remote_platform_workspace;

    let environment_work = if role_owns_managed_execution(role) {
        Some(
            open_environment_work(
                &coordinator_cfg.sessions,
                postgres_schema,
                clock.clone(),
                postgres_pools,
            )
            .await?,
        )
    } else {
        None
    };

    let coordinator = if role_owns_managed_execution(role) {
        ensure_store_parent(&coordinator_cfg.captured_content)?;
        let application_access = open_application_access(
            &coordinator_cfg.sessions,
            postgres_schema,
            clock.clone(),
            postgres_pools,
        )
        .await?;
        let SessionStoreViews {
            sessions,
            deployments,
            memory_extractions,
            dream_process_store,
        } = session_views
            .take()
            .expect("Managed Execution opens the canonical Session repository first");
        let (capture_sink, captured_content_eraser): (
            Arc<dyn awaken_runtime_contract::CaptureSink>,
            Arc<dyn awaken_data_governance_contract::ContentEraser>,
        ) = match &coordinator_cfg.captured_content {
            StoreBackend::Sqlite(p) => {
                let store = Arc::new(
                    awaken_captured_content_store::SqliteCapturedContentStore::open(
                        &path(p),
                        clock.clone(),
                    )
                    .await
                    .map_err(|error| {
                        format!("open captured-content SQLite {}: {error}", p.display())
                    })?,
                );
                (store.clone(), store)
            }
            StoreBackend::Postgres(url) => {
                let store = Arc::new(match postgres_schema {
                    PostgresSchemaMode::Migrate => {
                        awaken_captured_content_store::PgCapturedContentStore::with_pool(postgres_pools.get(url).await?, clock.clone()).await
                    }
                    PostgresSchemaMode::Verify => {
                        awaken_captured_content_store::PgCapturedContentStore::with_existing_pool(postgres_pools.get(url).await?, clock.clone())
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
            application_access,
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
        ensure_store_parent(&cfg.config)?;
        Some(match &cfg.config {
            StoreBackend::Sqlite(p) => Arc::new(
                awaken_config_store::SqliteConfigStore::open(&path(p))
                    .await
                    .map_err(|error| format!("open config SQLite {}: {error}", p.display()))?,
            ),
            StoreBackend::Postgres(url) => Arc::new(
                match postgres_schema {
                    PostgresSchemaMode::Migrate => {
                        awaken_config_store::PostgresConfigStore::with_pool(
                            postgres_pools.get(url).await?,
                        )
                        .await
                    }
                    PostgresSchemaMode::Verify => {
                        awaken_config_store::PostgresConfigStore::with_existing_pool(
                            postgres_pools.get(url).await?,
                        )
                        .await
                    }
                }
                .map_err(|error| format!("connect config Postgres: {error}"))?,
            ),
        })
    } else {
        None
    };

    let data_subject = if opens_control {
        ensure_store_parent(&cfg.data_subject)?;
        let repo: Arc<dyn awaken_data_subject_application::DataSubjectRepo>;
        let jobs: Arc<dyn awaken_data_subject_application::ErasureJobRepo>;
        match &cfg.data_subject {
            StoreBackend::Sqlite(p) => {
                let store = Arc::new(
                    awaken_data_subject_store::SqliteDataSubjectRepo::open(&path(p))
                        .await
                        .map_err(|error| {
                            format!("open data-subject SQLite {}: {error}", p.display())
                        })?,
                );
                repo = store.clone();
                jobs = store;
            }
            StoreBackend::Postgres(url) => {
                let store = Arc::new(
                    match postgres_schema {
                        PostgresSchemaMode::Migrate => {
                            awaken_data_subject_store::PgDataSubjectRepo::with_pool(
                                postgres_pools.get(url).await?,
                            )
                            .await
                        }
                        PostgresSchemaMode::Verify => {
                            awaken_data_subject_store::PgDataSubjectRepo::with_existing_pool(
                                postgres_pools.get(url).await?,
                            )
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
        ensure_store_parent(&cfg.environment)?;
        Some(match &cfg.environment {
            StoreBackend::Sqlite(environment_path) => (
                Arc::new(
                    awaken_env_store::SqliteEnvRegistry::open(
                        &path(environment_path),
                        clock.clone(),
                    )
                    .await
                    .map_err(|error| format!("open environments SQLite: {error}"))?,
                ) as Arc<dyn awaken_environment_contract::EnvRegistry>,
                Arc::new(
                    awaken_sandbox_policy_store::SqliteSandboxExecutionPolicyStore::open(&path(
                        environment_path,
                    ))
                    .await
                    .map_err(|error| format!("open sandbox policies SQLite: {error}"))?,
                )
                    as Arc<dyn awaken_provisioning_contract::SandboxExecutionPolicyStore>,
            ),
            StoreBackend::Postgres(url) => {
                let environments = match postgres_schema {
                    PostgresSchemaMode::Migrate => {
                        awaken_env_store::PostgresEnvRegistry::with_pool(
                            postgres_pools.get(url).await?,
                            clock.clone(),
                        )
                        .await
                    }
                    PostgresSchemaMode::Verify => {
                        awaken_env_store::PostgresEnvRegistry::with_existing_pool(
                            postgres_pools.get(url).await?,
                            clock.clone(),
                        )
                        .await
                    }
                }
                .map_err(|error| format!("connect environments Postgres: {error}"))?;
                let sandbox_policies = match postgres_schema {
                    PostgresSchemaMode::Migrate => {
                        awaken_sandbox_policy_store::PostgresSandboxExecutionPolicyStore::with_pool(
                            postgres_pools.get(url).await?,
                        )
                        .await
                    }
                    PostgresSchemaMode::Verify => {
                        awaken_sandbox_policy_store::PostgresSandboxExecutionPolicyStore::with_existing_pool(postgres_pools.get(url).await?).await
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

    // Publish or reuse the one local installation coordinate only after every
    // owned store has opened. A crash before this point leaves a store-first
    // state that only an explicit initialization retry may complete; a marker
    // created by Control/ACP remains insufficient evidence for Coordinator until
    // its canonical Session store has also opened.
    let platform_workspace = match platform_workspace.take() {
        Some(workspace) if publish_local_workspace => {
            awaken_installation_workspace::publish_at(&workspace_root, &workspace).map_err(
                |error| {
                    format!(
                        "publish platform Workspace identity under {}: {error}",
                        workspace_root.display()
                    )
                },
            )?
        }
        Some(workspace) => workspace,
        None => awaken_installation_workspace::provision_at(
            &workspace_root,
            SharedHost::mint_local_workspace,
        ),
    };
    Ok(ProcessStores {
        workspace_root: Some(workspace_root),
        platform_workspace,
        clock,
        control: if opens_control {
            let (environments, sandbox_policies) =
                control_environment.expect("Control role opens Environment stores");
            let admin = admin_views.expect("Control role opens admin store");
            Some(ControlStores {
                catalog: catalog.expect("Control role opens Catalog"),
                credentials: credentials.expect("Control role opens CredentialRepo"),
                secrets: secrets.expect("Control role opens SecretStore"),
                mcp_config: admin.mcp,
                profiles: admin.profiles,
                resources: admin.resources,
                webhooks: admin.webhooks,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MigrationManifest {
    role: config::Role,
}

impl MigrationManifest {
    /// Migration ownership is a projection of the canonical startup manifest,
    /// not a second per-role table. LocalWorker deliberately has no database
    /// migration component.
    pub(super) const fn contains(self, component: &MigrationComponent) -> bool {
        let startup_component = match component {
            MigrationComponent::Control => StartupComponent::Control,
            MigrationComponent::Coordinator => StartupComponent::Coordinator,
            MigrationComponent::Resources => StartupComponent::Resources,
        };
        self.role.owns_startup_component(startup_component)
    }
}

pub(super) const fn migration_manifest(role: config::Role) -> MigrationManifest {
    MigrationManifest { role }
}

pub(super) const fn role_owns_control_component(role: config::Role) -> bool {
    role.owns_startup_component(StartupComponent::Control)
}

pub(super) const fn role_owns_managed_execution(role: config::Role) -> bool {
    role.owns_startup_component(StartupComponent::Coordinator)
}

#[cfg(test)]
pub(super) const fn role_hosts_resources(role: config::Role) -> bool {
    role.owns_startup_component(StartupComponent::Resources)
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
        let control = migration_manifest(config::Role::Control);
        assert!(control.contains(&MigrationComponent::Control), "R1");
        assert!(!control.contains(&MigrationComponent::Resources), "R1");
        assert!(!control.contains(&MigrationComponent::Coordinator), "R1");

        let coordinator = migration_manifest(config::Role::Coordinator);
        assert!(!coordinator.contains(&MigrationComponent::Control), "R2");
        assert!(coordinator.contains(&MigrationComponent::Resources), "R2");
        assert!(coordinator.contains(&MigrationComponent::Coordinator), "R2");

        let worker = migration_manifest(config::Role::Worker);
        assert!(!worker.contains(&MigrationComponent::Control), "R3");
        assert!(!worker.contains(&MigrationComponent::Resources), "R3");
        assert!(!worker.contains(&MigrationComponent::Coordinator), "R3");

        let all_in_one = migration_manifest(config::Role::AllInOne);
        assert!(all_in_one.contains(&MigrationComponent::Control), "R4");
        assert!(all_in_one.contains(&MigrationComponent::Resources), "R4");
        assert!(all_in_one.contains(&MigrationComponent::Coordinator), "R4");
    }
}
