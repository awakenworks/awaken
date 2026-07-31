//! Process adapters for the canonical Control application component.
//!
//! This module maps concrete product stores and provider adapters onto
//! `awaken-control` ports. It contains no Control business implementation.

use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) async fn control_component_for_process(
    stores: &ProcessStores,
    execution_workspace: &str,
    executable_agent_registrar: Arc<dyn awaken_executable_agent_contract::ExecutableAgentRegistrar>,
    model_publication_resolver: Arc<dyn awaken_config_service::ModelPublicationResolver>,
    web_search_publication_resolver: Arc<dyn awaken_config_service::PluginPublicationResolver>,
    web_search_providers: &awaken_ext_builtin_tools::WebSearchProviderRegistry,
    brokered_client: Option<Arc<awaken_server::brokered_inference::HttpBrokeredInferenceClient>>,
    injected_brokered_catalog: Option<Arc<dyn awaken_admin_config_api::BrokeredCatalogDiscovery>>,
    model_supply: awaken_admin_config_api::ModelSupplyCapabilityView,
    local_acp_observations: &[awaken_acp_application::AcpHostObservation],
    runtimes: Arc<dyn awaken_config_service::RuntimeCapabilitySource>,
    resource_inventory: Option<Arc<dyn awaken_admin_assistant::ResourceInventory>>,
    iam: Option<Arc<ManagementAuthz>>,
    local_browser_auth: Option<awaken_control::LocalBrowserAuth>,
    remote_iam: Option<Arc<RemoteManagementAuthz>>,
) -> awaken_control::ControlComponent {
    let assistant_catalog = stores.catalog.snapshot().await.unwrap_or_default();
    let assistant_credentials = stores
        .credentials
        .list(execution_workspace)
        .await
        .unwrap_or_default();
    let assistant_model_selection = assistant_selection::select(
        &assistant_catalog,
        &assistant_credentials,
        local_acp_observations,
    );
    awaken_control::build_control_component(awaken_control::ControlDependencies {
        execution_workspace: execution_workspace.to_owned(),
        catalog: stores.catalog.clone(),
        credentials: stores.credentials.clone(),
        secrets: stores.secrets.clone(),
        profiles: stores.profiles.clone(),
        webhook_store: stores.webhooks.clone(),
        resource_store: stores.resources.clone(),
        config_store: stores.config.clone(),
        executable_agent_registrar,
        model_publication_resolver,
        plugin_publication_resolvers: vec![web_search_publication_resolver],
        credential_probe: Arc::new(credential_probe::GenaiProbe),
        model_discovery: Arc::new(awaken_server::model_discovery::GenaiModelDiscovery::new(
            stores.secrets.clone(),
        )),
        brokered_catalog: injected_brokered_catalog.or_else(|| {
            brokered_client
                .map(|client| client as Arc<dyn awaken_admin_config_api::BrokeredCatalogDiscovery>)
        }),
        model_supply,
        mcp_probe: Some(Arc::new(ExtMcpProbe)),
        assistant_model_selection,
        global_tools: awaken_runtime_host::authorable_tools(),
        platform_plugins: awaken_runtime_host::platform_plugin_capabilities_with_web_search(
            web_search_providers,
        ),
        assistant_plugins: awaken_runtime_host::authorable_config_sections_with_web_search(
            web_search_providers,
        ),
        runtimes,
        resource_inventory,
        environment_author: Arc::new(awaken_control::EnvironmentStateAuthor::new(
            stores.environments.clone(),
        )),
        iam,
        local_browser_auth,
        remote_iam,
    })
    .await
    .unwrap_or_else(|error| panic!("build Control component: {error}"))
}

/// Standalone Control process assembly. It opens no Managed Execution path and
/// asks the same Control component builder used by AllInOne for the complete
/// authoring application.
pub(super) async fn assemble_control_process_router(
    stores: ProcessStores,
    iam: Option<Arc<ManagementAuthz>>,
    remote_iam: Option<Arc<RemoteManagementAuthz>>,
    local_browser_auth: Option<awaken_control::LocalBrowserAuth>,
    model_composition: PublicationModelComposition,
    assembly: ProcessAssemblyOptions,
) -> Router {
    debug_assert_eq!(assembly.role, config::Role::Control);
    let (_, executable_agent_registrar, _, _) =
        executable_agent_registration::process_parts(assembly.executable_agent_wiring);
    let execution_workspace = stores.workspace_root.as_deref().map_or_else(
        SharedHost::provision_local_workspace,
        SharedHost::provision_local_workspace_at,
    );
    let model_supply = assembly.model_supply.clone();
    let cloud_models_enabled = model_supply.cloud_models_enabled;
    let brokered_client = brokered_inference_client(
        cloud_models_enabled,
        remote_iam.as_ref(),
        assembly.cloud_api_base_url.as_deref(),
        &execution_workspace,
    );
    let model_assembly =
        publication_model_assembly(model_composition, &stores, cloud_models_enabled);
    let web_search_providers = assembly
        .web_search_providers
        .unwrap_or_else(awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins);
    let web_search_publication_resolver =
        assembly.web_search_publication_resolver.unwrap_or_else(|| {
            Arc::new(awaken_config_service::WebSearchPublicationResolver::new(
                web_search_providers.clone(),
            ))
        });
    let runtimes = Arc::new(LiveRuntimeCapabilities {
        initial: assembly.local_acp_observations.clone(),
        workers: awaken_server::worker_directory(),
        credentials: stores.credentials.clone(),
        workspace: execution_workspace.clone(),
    });
    let component = control_component_for_process(
        &stores,
        &execution_workspace,
        executable_agent_registrar,
        model_assembly.publication_resolver,
        web_search_publication_resolver,
        &web_search_providers,
        brokered_client,
        assembly.brokered_catalog,
        model_supply,
        &assembly.local_acp_observations,
        runtimes,
        None,
        iam,
        local_browser_auth,
        remote_iam,
    )
    .await;
    let mcp_export = awaken_server::mcp_export::router(
        awaken_admin_assistant::admin_tool_descriptors(),
        component.admin_tools,
        assembly.mcp_bearer_token,
    );
    process_surface::finish(
        component.router,
        mcp_export,
        Some(component.publication_reconciler),
        execution_workspace,
        Arc::new(
            awaken_protocol_managed::ManagedRateLimiter::for_organization(
                assembly.org_id.unwrap_or_else(local_org_id),
            ),
        ),
    )
}
