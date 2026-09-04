//! Control process startup over the canonical deployment stores.
//!
//! This module owns only the control-only process choice. Router, IAM,
//! ConfigService, publication persistence, and schema ownership remain in their
//! existing modules.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;

use super::*;

const HOSTED_MODEL_CATALOG_RECONCILIATION_INTERVAL: Duration = Duration::from_secs(60);

fn spawn_hosted_model_catalog_reconciliation(
    catalog: Arc<dyn awaken_model_catalog::repo::CatalogRepo>,
    discovery: Arc<dyn awaken_admin_config_api::BrokeredCatalogDiscovery>,
    service_lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
) {
    service_lifecycle.spawn("control-hosted-model-catalog", move |cancel| async move {
        let mut interval = tokio::time::interval(HOSTED_MODEL_CATALOG_RECONCILIATION_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = interval.tick() => {}
            }
            if let Err(error) = awaken_admin_config_api::reconcile_brokered_catalog(
                discovery.as_ref(),
                catalog.as_ref(),
            )
            .await
            {
                eprintln!("hosted model catalog reconciliation failed: {error}");
            }
        }
        Ok(())
    });
}

/// Canonical hosted authoring/control process. It reuses the same stores,
/// resolver, IAM PEP and routes as the full local product, but deliberately
/// omits every Session/Run/protocol/Worker data-plane route.
pub async fn build_control_router(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
) -> Result<Router, String> {
    prepare_control_process(deployment, key)
        .await
        .map(|process| process.public_router)
}

/// Canonical hosted process, including the one-time local setup handoff owned
/// by the shared identity wiring. The router-only entry point projects this
/// value instead of maintaining a second startup path.
pub async fn prepare_control_process(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
) -> Result<PreparedProcess, String> {
    prepare_control_process_with_model_supply(
        deployment,
        key,
        PublicationModelSupply::PublishedProviders,
        None,
        None,
        None,
        ManagedServiceAdapters::default(),
    )
    .await
}

/// Canonical hosted control process with a deployment-owned provider
/// publication resolver.
///
/// Awaken continues to own Agent authoring, compilation, fingerprinting, and
/// publication persistence. A closed deployment supplies only the existing
/// [`ModelPublicationResolver`](awaken_config_service::ModelPublicationResolver)
/// interface; it does not replace the router, config service, or publication store.
pub async fn build_control_router_with_publication_resolver(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
    resolver: Arc<dyn awaken_config_service::ModelPublicationResolver>,
) -> Result<Router, String> {
    prepare_control_process_with_model_supply(
        deployment,
        key,
        PublicationModelSupply::HostedPublication {
            resolver,
            direct_credential_execution: None,
        },
        None,
        None,
        None,
        ManagedServiceAdapters::default(),
    )
    .await
    .map(|process| process.public_router)
}

/// Canonical hosted Control process with a deployment-owned provider
/// publication resolver and the built-in WebSearch publication policy.
pub async fn prepare_control_process_with_publication_resolver(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
    resolver: Arc<dyn awaken_config_service::ModelPublicationResolver>,
) -> Result<PreparedProcess, String> {
    prepare_control_process_with_model_supply(
        deployment,
        key,
        PublicationModelSupply::HostedPublication {
            resolver,
            direct_credential_execution: None,
        },
        None,
        None,
        None,
        ManagedServiceAdapters::default(),
    )
    .await
}

/// Hosted control process with deployment-owned model and WebSearch
/// publication adapters. This remains the same canonical Control process;
/// the closed startup supplies only existing open SPIs and provider facts.
pub async fn build_control_router_with_publication_resolver_and_web_search(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
    resolver: Arc<dyn awaken_config_service::ModelPublicationResolver>,
    brokered_catalog: Option<Arc<dyn awaken_admin_config_api::BrokeredCatalogDiscovery>>,
    web_search_providers: awaken_ext_builtin_tools::WebSearchProviderRegistry,
    web_search_publication_resolver: Arc<dyn awaken_config_service::PluginPublicationResolver>,
) -> Result<Router, String> {
    prepare_control_process_with_publication_resolver_and_web_search(
        deployment,
        key,
        resolver,
        brokered_catalog,
        web_search_providers,
        web_search_publication_resolver,
    )
    .await
    .map(|process| process.public_router)
}

/// Hosted control process with deployment-owned publication adapters and both
/// role-owned listener surfaces. The public and private routers remain
/// separate; embedders must expose the private router only on an internal
/// listener protected by the configured service authenticator.
pub async fn prepare_control_process_with_publication_resolver_and_web_search(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
    resolver: Arc<dyn awaken_config_service::ModelPublicationResolver>,
    brokered_catalog: Option<Arc<dyn awaken_admin_config_api::BrokeredCatalogDiscovery>>,
    web_search_providers: awaken_ext_builtin_tools::WebSearchProviderRegistry,
    web_search_publication_resolver: Arc<dyn awaken_config_service::PluginPublicationResolver>,
) -> Result<PreparedProcess, String> {
    prepare_control_process_with_publication_resolver_web_search_and_lifecycle_delivery(
        deployment,
        key,
        resolver,
        brokered_catalog,
        web_search_providers,
        web_search_publication_resolver,
        None,
    )
    .await
}

/// Hosted control process with the canonical publication adapters plus one
/// deployment-owned lifecycle receiver. The existing durable lifecycle outbox
/// remains the only source and retry authority for every receiver.
pub async fn prepare_control_process_with_publication_resolver_web_search_and_lifecycle_delivery(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
    resolver: Arc<dyn awaken_config_service::ModelPublicationResolver>,
    brokered_catalog: Option<Arc<dyn awaken_admin_config_api::BrokeredCatalogDiscovery>>,
    web_search_providers: awaken_ext_builtin_tools::WebSearchProviderRegistry,
    web_search_publication_resolver: Arc<dyn awaken_config_service::PluginPublicationResolver>,
    additional_lifecycle_delivery: Option<Arc<dyn awaken_session_contract::LifecycleFactDelivery>>,
) -> Result<PreparedProcess, String> {
    prepare_control_process_with_managed_services(
        deployment,
        key,
        resolver,
        brokered_catalog,
        web_search_providers,
        web_search_publication_resolver,
        additional_lifecycle_delivery,
        ManagedServiceAdapters::default(),
    )
    .await
}

/// Hosted Control composition with the optional Cloud-only Managed adapters.
/// Tunnel routes enter the canonical IAM/audit edge, and the same injected
/// organization limiter wraps every public Managed route.
#[allow(clippy::too_many_arguments)]
pub async fn prepare_control_process_with_managed_services(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
    resolver: Arc<dyn awaken_config_service::ModelPublicationResolver>,
    brokered_catalog: Option<Arc<dyn awaken_admin_config_api::BrokeredCatalogDiscovery>>,
    web_search_providers: awaken_ext_builtin_tools::WebSearchProviderRegistry,
    web_search_publication_resolver: Arc<dyn awaken_config_service::PluginPublicationResolver>,
    additional_lifecycle_delivery: Option<Arc<dyn awaken_session_contract::LifecycleFactDelivery>>,
    managed_services: ManagedServiceAdapters,
) -> Result<PreparedProcess, String> {
    let direct_credential_execution = managed_services.provider_credential_execution.clone();
    prepare_control_process_with_model_supply(
        deployment,
        key,
        PublicationModelSupply::HostedPublication {
            resolver,
            direct_credential_execution,
        },
        brokered_catalog,
        Some((web_search_providers, web_search_publication_resolver)),
        additional_lifecycle_delivery,
        managed_services,
    )
    .await
}

async fn prepare_control_process_with_model_supply(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
    model_supply: PublicationModelSupply,
    brokered_catalog: Option<Arc<dyn awaken_admin_config_api::BrokeredCatalogDiscovery>>,
    web_search: Option<(
        awaken_ext_builtin_tools::WebSearchProviderRegistry,
        Arc<dyn awaken_config_service::PluginPublicationResolver>,
    )>,
    additional_lifecycle_delivery: Option<Arc<dyn awaken_session_contract::LifecycleFactDelivery>>,
    managed_services: ManagedServiceAdapters,
) -> Result<PreparedProcess, String> {
    let installations = installation_binding::verify_deployment_installations(deployment).await?;
    prepare_control_process_with_model_supply_and_installations(
        deployment,
        key,
        model_supply,
        brokered_catalog,
        web_search,
        additional_lifecycle_delivery,
        managed_services,
        installations,
    )
    .await
}

pub(super) async fn prepare_control_process_with_installations(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
    installations: installation_binding::PreparedDeploymentInstallations,
) -> Result<PreparedProcess, String> {
    prepare_control_process_with_model_supply_and_installations(
        deployment,
        key,
        PublicationModelSupply::PublishedProviders,
        None,
        None,
        None,
        ManagedServiceAdapters::default(),
        installations,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn prepare_control_process_with_model_supply_and_installations(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
    model_supply: PublicationModelSupply,
    brokered_catalog: Option<Arc<dyn awaken_admin_config_api::BrokeredCatalogDiscovery>>,
    web_search: Option<(
        awaken_ext_builtin_tools::WebSearchProviderRegistry,
        Arc<dyn awaken_config_service::PluginPublicationResolver>,
    )>,
    additional_lifecycle_delivery: Option<Arc<dyn awaken_session_contract::LifecycleFactDelivery>>,
    mut managed_services: ManagedServiceAdapters,
    installations: installation_binding::PreparedDeploymentInstallations,
) -> Result<PreparedProcess, String> {
    let publish_local_workspace = installations.publishes_local_workspace();
    let platform_workspace = installations.platform_workspace_before_write()?;
    let service_lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
    let hosted_byok_enabled = managed_services.provider_credential_execution.is_some();
    let background_services = std::mem::take(&mut managed_services.background_services);
    let stores = open_process_stores(ProcessStoreOpenOptions {
        control: deployment.control.clone(),
        coordinator: deployment.coordinator.clone(),
        // Control receives Resource references through authoring/read ports and
        // opens no File, Memory, Skill-content, or lifecycle authority.
        resources: None,
        workspace_root: deployment.data_dir.clone(),
        platform_workspace,
        publish_local_workspace,
        seal_key: Some(key),
        role: config::Role::Control,
        postgres_schema: PostgresSchemaMode::Verify,
    })
    .await?;
    let identity = identity_wiring(
        deployment,
        &stores.platform_workspace,
        awaken_iam_client::CredentialCache::open(),
        managed_services.entitlement_provider.take(),
    )
    .await?;
    let catalog = stores
        .control
        .as_ref()
        .expect("Control role opens Control stores")
        .catalog
        .clone();
    if let Some(discovery) = brokered_catalog.as_ref() {
        awaken_admin_config_api::reconcile_brokered_catalog(discovery.as_ref(), catalog.as_ref())
            .await
            .map_err(|error| {
                format!("initial hosted model catalog reconciliation failed: {error}")
            })?;
        spawn_hosted_model_catalog_reconciliation(catalog, discovery.clone(), &service_lifecycle);
    }
    let executable_agent_wiring =
        executable_agent_registration::ExecutableAgentWiring::control(deployment)?;
    let executable_environment_wiring =
        executable_environment_registration::ExecutableEnvironmentWiring::control(deployment)?;
    let worker_observations =
        worker_observation_wiring::WorkerObservationWiring::control(deployment)?;
    let prepared = prepare_control_routers(
        stores,
        identity.iam,
        identity.remote_iam,
        identity.local_browser_auth,
        model_supply,
        ProcessStartup {
            service_lifecycle: service_lifecycle.clone(),
            deployment: None,
            content_capture_ceiling: deployment.runtime.content_capture.level,
            org_id: Some(deployment.org_id.clone()),
            enrollment_signing_key: Some(
                awaken_data_subject_application::derive_enrollment_signing_key(key),
            ),
            mcp_bearer_token: deployment.mcp_bearer_token.clone(),
            ai_sdk_browser_cors: awaken_coordinator::AiSdkBrowserCors::default(),
            role: config::Role::Control,
            cloud_api_base_url: Some(deployment.cloud_iam.inference_base_url.clone()),
            cloud_developer_key_file: deployment.cloud_iam.developer_key_file.clone(),
            model_supply: awaken_admin_config_api::ModelSupplyCapabilityView {
                local_catalog_enabled: hosted_byok_enabled,
                byok_enabled: hosted_byok_enabled,
                cloud_models_enabled: true,
                profile_authoring_enabled: hosted_byok_enabled,
            },
            brokered_catalog,
            cloud_login: identity.cloud_login,
            local_acp_observations: Vec::new(),
            web_search_providers: web_search.as_ref().map(|value| value.0.clone()),
            web_search_publication_resolver: web_search.map(|value| value.1),
            executable_agent_wiring: Some(executable_agent_wiring),
            executable_environment_wiring: Some(executable_environment_wiring),
            worker_authenticator: None,
            worker_placement_policy: None,
            cloud_native_credential_realization: None,
            repository_transport_authorizer: None,
            inference_materializer: None,
            worker_directory: None,
            runtime_authority: None,
            worker_observations: Some(worker_observations),
            control_service_authenticator: Some(
                deployment.control_service.control_authenticator()?,
            ),
            control_service: None,
            additional_lifecycle_delivery,
            managed_services,
        },
    )
    .await?;
    managed_platform::install_background_services(
        &prepared.service_lifecycle,
        &background_services,
    );
    Ok(PreparedProcess {
        public_router: prepared.public_router,
        private_router: prepared.private_router,
        local_setup: identity.local_setup,
        registration_supervisor: prepared.registration_supervisor,
        service_lifecycle: prepared.service_lifecycle,
        event_batch_cutover_validation: prepared.event_batch_cutover_validation,
        coordinator_authorities: None,
        admin_tools: prepared.admin_tools,
    })
}
