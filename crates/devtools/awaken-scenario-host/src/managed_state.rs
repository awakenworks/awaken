//! Scenario composition of the Coordinator's Managed state over a co-located
//! Session executor.
//!
//! The Coordinator links no executor, so the scenario root builds it here:
//! one executor, one Session application over it, the executor's edges bound
//! back into that application, and the Coordinator's execution ports
//! selected through the same [`awaken_process_startup::coordinator_execution`] the
//! product binary uses.

use std::sync::Arc;

use awaken_session_executor::{ExecutorComponents, SessionExecutorBuilder};

/// The executor a scenario built and the Coordinator's Managed state over it.
pub struct LocalManagedState {
    /// The co-located executor; scenarios drive and inspect it directly.
    pub host: Arc<ExecutorComponents>,
    /// What the Coordinator's scenario mounts consume.
    pub state: awaken_coordinator::ScenarioManagedState,
}

/// Assemble the local/single-process Managed adapter with one shared ephemeral
/// credential plane. Repository tokens are sealed immediately and runtime receives
/// only a credential reference. Production processes replace these
/// in-memory adapters with their durable equivalents; resource services remain
/// unaware of principals, API keys, roles, or authorization policy.
pub async fn local_managed_state(
    dependencies: SessionExecutorBuilder,
    catalog: Arc<dyn awaken_resource_contract::ResourceRegistry>,
) -> LocalManagedState {
    local_managed_state_over(dependencies, catalog, None, None, None, None).await
}

/// Scenario variant with the same Session model-publication resolver used by
/// production. It exists for provider-backed fixtures whose public model
/// reference differs from the built-in Agent's deterministic default.
pub async fn local_managed_state_with_model_publication_resolver(
    dependencies: SessionExecutorBuilder,
    catalog: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    resolver: Arc<dyn awaken_session_configuration_contract::SessionModelPublicationResolver>,
) -> LocalManagedState {
    local_managed_state_over(dependencies, catalog, None, None, Some(resolver), None).await
}

/// Scenario variant that freezes both the immutable Agent publication and the
/// model-reference resolver. Model-routing fixtures need both authorities: the
/// resolver selects a candidate, while the Agent snapshot proves its backend.
pub async fn local_managed_state_with_agent_source_and_model_publication_resolver(
    dependencies: SessionExecutorBuilder,
    catalog: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    agent_source: Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>,
    resolver: Arc<dyn awaken_session_configuration_contract::SessionModelPublicationResolver>,
) -> LocalManagedState {
    local_managed_state_over(
        dependencies,
        catalog,
        None,
        Some(agent_source),
        Some(resolver),
        None,
    )
    .await
}

/// [`local_managed_state`] with one immutable Agent projection source. Embedded
/// scenario hosts use this to exercise publication-owned routing without mounting
/// a second authoring plane.
pub async fn local_managed_state_with_agent_source(
    dependencies: SessionExecutorBuilder,
    catalog: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    agent_source: Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>,
) -> LocalManagedState {
    local_managed_state_over(dependencies, catalog, None, Some(agent_source), None, None).await
}

/// [`local_managed_state_with_agent_source`] with the exact list-price
/// authority needed by budgeted Managed Session fixtures. The authority is
/// installed before sharing, so callers never unwrap or mutate an `Arc`.
pub async fn local_managed_state_with_agent_source_and_list_prices(
    dependencies: SessionExecutorBuilder,
    catalog: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    agent_source: Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>,
    list_prices: Arc<dyn awaken_session_budget_contract::ManagedListPriceProvider>,
) -> LocalManagedState {
    local_managed_state_over(
        dependencies,
        catalog,
        None,
        Some(agent_source),
        None,
        Some(list_prices),
    )
    .await
}

/// [`local_managed_state`] with the Environment registry/work queue installed on
/// the same Managed aggregate. Processes that mount `/v1/environments`
/// must pass that exact state here so Session environment pins resolve through the
/// registry that authored them.
pub async fn local_managed_state_with_environments(
    dependencies: SessionExecutorBuilder,
    catalog: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    environments: Arc<awaken_environment_application::execution::EnvironmentExecutionApplication>,
) -> LocalManagedState {
    local_managed_state_over(dependencies, catalog, Some(environments), None, None, None).await
}

/// Scenario/local variant that installs the same Agent projection
/// used by production before the Managed aggregate starts its supervisors.
pub async fn local_managed_state_with_environments_and_agent_source(
    dependencies: SessionExecutorBuilder,
    catalog: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    environments: Arc<awaken_environment_application::execution::EnvironmentExecutionApplication>,
    agent_source: Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>,
) -> LocalManagedState {
    local_managed_state_over(
        dependencies,
        catalog,
        Some(environments),
        Some(agent_source),
        None,
        None,
    )
    .await
}

/// The volatile Environment execution projection scenario startup uses when
/// the caller supplies none: one in-memory WorkQueue and a catalog seeded
/// with the built-in local Environment.
pub fn volatile_environment_execution(
    clock: Arc<dyn awaken_time::WallClock>,
) -> Arc<awaken_environment_application::execution::EnvironmentExecutionApplication> {
    let catalog =
        Arc::new(awaken_executable_environment_catalog::ExecutableEnvironmentCatalog::new());
    catalog
        .install_seed(awaken_environment_application::default_environment_registration())
        .expect("install built-in local Environment");
    Arc::new(
        awaken_environment_application::execution::EnvironmentExecutionApplication::new(
            Arc::new(awaken_work_store::InMemoryWorkQueue::new(clock)),
            catalog,
        ),
    )
}

async fn local_managed_state_over(
    dependencies: SessionExecutorBuilder,
    catalog: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    environments: Option<
        Arc<awaken_environment_application::execution::EnvironmentExecutionApplication>,
    >,
    agent_source: Option<Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>>,
    model_publication_resolver: Option<
        Arc<dyn awaken_session_configuration_contract::SessionModelPublicationResolver>,
    >,
    managed_list_price_provider: Option<
        Arc<dyn awaken_session_budget_contract::ManagedListPriceProvider>,
    >,
) -> LocalManagedState {
    // The Session application runs on the Host's clock, so Session facts and
    // the dispatch they drive share one timeline.
    let clock = dependencies.clock();
    let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
    let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
    let vaults = Arc::new(awaken_protocol_managed::VaultState::new(
        secrets.clone(),
        credentials.clone(),
        clock.clone(),
        None,
    ));
    // The Session aggregate lives in the same `sessions.db` as the Memory
    // extraction intents: the composition that built this Host selected its
    // extraction repository over that file before construction. The two
    // application SPIs remain separate, while their local durability boundary
    // is shared at this process boundary.
    let durable_session_repo = match dependencies.storage_dir() {
        Some(root) => {
            std::fs::create_dir_all(root).expect("create durable session repository directory");
            Some(Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open(
                    &root.join("sessions.db").to_string_lossy(),
                )
                .await
                .expect("open durable managed session repository"),
            ))
        }
        None => None,
    };
    let session_repo: Arc<dyn awaken_session_repository_contract::SessionStore> =
        match durable_session_repo {
            Some(repo) => repo,
            None => Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                    .await
                    .expect("open ephemeral managed Session repository"),
            ),
        };

    let persistence =
        awaken_session_application::SessionPersistence::new(session_repo, clock.clone());
    let repository_bindings = Arc::new(
        awaken_resource_application::RegistryRepositoryBindingVerifier::new(catalog.clone()),
    );
    let dependencies = dependencies
        .with_session_execution(
            awaken_session_executor::SessionExecutionDependencies::default()
                .with_resource_validator(catalog.clone())
                .with_repository_binding_verifier(repository_bindings),
        )
        .with_credential_materializer(
            awaken_credential_materializer::PinnedCredentialMaterializer::new(
                Arc::new(awaken_credential_vault::VaultLocalMaterialAuthority::new(
                    credentials,
                    secrets,
                    clock.clone(),
                )),
                clock.clone(),
            ),
        );

    let host = (dependencies.with_environment_binding_sink(persistence.environment_binding_sink()))
        .build()
        .expect("validated Host dependencies");

    // Test/scenario startup must freeze the same topology fact as product
    // startup. Deriving placement from the canonical Host value eliminates
    // the former second path where ManagedState always selected LocalWorker even
    // when this process deliberately had no local dispatch pool.
    let execution_placement = if host.dispatch().topology().local.runs_local_pool() {
        awaken_session_application::SessionExecutionPlacement::LocalWorker
    } else {
        awaken_session_application::SessionExecutionPlacement::RegisteredWorker
    };
    let local_realization_owner = host.dispatch().topology().lease_owner.as_str().to_owned();
    let environments =
        environments.unwrap_or_else(|| volatile_environment_execution(clock.clone()));
    let mut builder = awaken_session_application::SessionApplicationBuilder::new(
        awaken_session_executor::CoordinatorRoles::of(&host).session_execution(),
        persistence,
        environments.clone(),
        awaken_session_application::SessionExecutionAdmission::fixture_with_dispatch(
            host.coordination()
                .dispatch_observation_source()
                .expect("fixture canonical dispatch feed"),
        ),
        Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .configuration(
        awaken_session_application::SessionApplicationConfiguration {
            execution_placement,
            local_realization_owner,
            ..Default::default()
        },
    )
    .credentials(
        awaken_session_application::SessionCredentialAuthorities::read_write(
            vaults.application().credential_access(),
        ),
    )
    .resources(awaken_session_application::SessionResources::catalog(
        catalog,
    ));
    if let Some(source) = agent_source {
        builder = builder.agent_profiles(source);
    }
    if let Some(resolver) = model_publication_resolver {
        builder = builder.model_overrides(resolver);
    }
    if let Some(provider) = managed_list_price_provider {
        builder = builder.managed_prices(provider);
    }
    let application = builder
        .build()
        .expect("scenario Session capabilities are complete");
    let application = Arc::new(application);
    let execution = awaken_process_startup::coordinator_execution(&host, &application)
        .expect("scenario Session application binds into its executor once");
    let managed = awaken_protocol_managed::ManagedState::from_application(
        application.clone(),
        host.content_capture_policy(),
    );
    LocalManagedState {
        state: awaken_coordinator::ScenarioManagedState {
            execution,
            managed: Arc::new(managed),
            sessions: application,
            environment_warmups: environments,
            local_workspace: host.local_workspace().to_owned(),
            clock: host.clock(),
        },
        host,
    }
}
