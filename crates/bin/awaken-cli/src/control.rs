//! Control process composition over the canonical deployment stores.
//!
//! This module owns only the control-only assembly choice. Router, IAM,
//! ConfigService, publication persistence, and schema ownership remain in their
//! existing modules.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;

use super::*;

const HOSTED_MODEL_CATALOG_RECONCILIATION_INTERVAL: Duration = Duration::from_secs(60);

fn spawn_hosted_model_catalog_reconciliation(
    catalog: Arc<dyn awaken_model_catalog::repo::CatalogRepo>,
    discovery: Arc<dyn awaken_admin_config_api::BrokeredCatalogDiscovery>,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(HOSTED_MODEL_CATALOG_RECONCILIATION_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(error) = awaken_admin_config_api::reconcile_brokered_catalog(
                discovery.as_ref(),
                catalog.as_ref(),
            )
            .await
            {
                eprintln!("hosted model catalog reconciliation failed: {error}");
            }
        }
    });
}

/// Canonical hosted authoring/control assembly. It reuses the same stores,
/// resolver, IAM PEP and routes as the full local product, but deliberately
/// omits every Session/Run/protocol/Worker data-plane route.
pub async fn build_control_router(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
) -> Result<Router, String> {
    build_control_assembly(deployment, key)
        .await
        .map(|assembly| assembly.router)
}

/// Canonical hosted assembly, including the one-time local setup handoff owned
/// by the shared identity wiring. The router-only entry point projects this
/// value instead of maintaining a second composition path.
pub async fn build_control_assembly(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
) -> Result<ProcessAssembly, String> {
    build_control_assembly_with_model_composition(
        deployment,
        key,
        PublicationModelComposition::PublishedProviders,
        None,
        None,
    )
    .await
}

/// Canonical hosted control assembly with a deployment-owned provider
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
    let providers = awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins();
    let publication_resolver = Arc::new(awaken_config_service::WebSearchPublicationResolver::new(
        providers.clone(),
    ));
    build_control_router_with_publication_resolver_and_web_search(
        deployment,
        key,
        resolver,
        None,
        providers,
        publication_resolver,
    )
    .await
}

/// Hosted control assembly with deployment-owned model and WebSearch
/// publication adapters. This remains the same canonical Control assembly;
/// the closed composition supplies only existing open SPIs and provider facts.
pub async fn build_control_router_with_publication_resolver_and_web_search(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
    resolver: Arc<dyn awaken_config_service::ModelPublicationResolver>,
    brokered_catalog: Option<Arc<dyn awaken_admin_config_api::BrokeredCatalogDiscovery>>,
    web_search_providers: awaken_ext_builtin_tools::WebSearchProviderRegistry,
    web_search_publication_resolver: Arc<dyn awaken_config_service::PluginPublicationResolver>,
) -> Result<Router, String> {
    build_control_assembly_with_model_composition(
        deployment,
        key,
        PublicationModelComposition::HostedPublication { resolver },
        brokered_catalog,
        Some((web_search_providers, web_search_publication_resolver)),
    )
    .await
    .map(|assembly| assembly.router)
}

async fn build_control_assembly_with_model_composition(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
    model_composition: PublicationModelComposition,
    brokered_catalog: Option<Arc<dyn awaken_admin_config_api::BrokeredCatalogDiscovery>>,
    web_search: Option<(
        awaken_ext_builtin_tools::WebSearchProviderRegistry,
        Arc<dyn awaken_config_service::PluginPublicationResolver>,
    )>,
) -> Result<ProcessAssembly, String> {
    let identity = identity_wiring(
        deployment.identity_mode,
        Some(&deployment.data_dir),
        &deployment.org_id,
        &deployment.iam_workspaces,
        &deployment.cloud_iam,
    )?;
    let stores = open_process_stores(ProcessStoreOpenOptions {
        control: deployment.control.clone(),
        coordinator: deployment.coordinator.clone(),
        // Control receives Resource references through authoring/read ports and
        // opens no File, Memory, Skill-content, or lifecycle authority.
        resource_component: None,
        workspace_root: deployment.data_dir.clone(),
        seal_key: Some(key),
        role: config::Role::Control,
        postgres_schema: PostgresSchemaMode::Verify,
    })
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
        spawn_hosted_model_catalog_reconciliation(catalog, discovery.clone());
    }
    let executable_agent_wiring =
        executable_agent_registration::ExecutableAgentWiring::control(deployment)?;
    let executable_environment_wiring =
        executable_environment_registration::ExecutableEnvironmentWiring::control(deployment)?;
    let assembled = assemble_control_process_router(
        stores,
        identity.iam,
        identity.remote_iam,
        identity.local_browser_auth,
        model_composition,
        ProcessAssemblyOptions {
            deployment: None,
            content_capture_ceiling: deployment.runtime.content_capture.level,
            org_id: Some(deployment.org_id.clone()),
            mcp_bearer_token: deployment.mcp_bearer_token.clone(),
            role: config::Role::Control,
            cloud_api_base_url: Some(deployment.cloud_iam.inference_base_url.clone()),
            model_supply: awaken_admin_config_api::ModelSupplyCapabilityView {
                local_catalog_enabled: false,
                byok_enabled: false,
                cloud_models_enabled: true,
                profile_authoring_enabled: false,
            },
            brokered_catalog,
            local_acp_observations: Vec::new(),
            web_search_providers: web_search.as_ref().map(|value| value.0.clone()),
            web_search_publication_resolver: web_search.map(|value| value.1),
            executable_agent_wiring: Some(executable_agent_wiring),
            executable_environment_wiring: Some(executable_environment_wiring),
            worker_authenticator: None,
            control_service_token: Some(deployment.control_service.control_token()?),
            control_service: None,
        },
    )
    .await;
    Ok(ProcessAssembly {
        router: assembled.router,
        local_setup: identity.local_setup,
        registration_supervisor: assembled.registration_supervisor,
    })
}
