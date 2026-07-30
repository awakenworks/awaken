//! Control process composition over the canonical deployment stores.
//!
//! This module owns only the control-only assembly choice. Router, IAM,
//! ConfigService, publication persistence, and schema ownership remain in their
//! existing modules.

use std::sync::Arc;

use axum::Router;

use super::*;

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
    )
    .await
}

/// Canonical hosted control assembly with a deployment-owned provider
/// publication resolver.
///
/// Awaken continues to own Agent authoring, compilation, fingerprinting, and
/// publication persistence. A closed deployment supplies only the existing
/// [`ModelPublicationResolver`](awaken_runtime_host::ModelPublicationResolver)
/// interface; it does not replace the router, config service, or publication store.
pub async fn build_control_router_with_publication_resolver(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
    resolver: Arc<dyn awaken_runtime_host::ModelPublicationResolver>,
) -> Result<Router, String> {
    let providers = awaken_runtime_host::WebSearchProviderRegistry::builtins();
    let publication_resolver = Arc::new(awaken_runtime_host::WebSearchPublicationResolver::new(
        providers.clone(),
    ));
    build_control_router_with_publication_resolver_and_web_search(
        deployment,
        key,
        resolver,
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
    resolver: Arc<dyn awaken_runtime_host::ModelPublicationResolver>,
    web_search_providers: awaken_runtime_host::WebSearchProviderRegistry,
    web_search_publication_resolver: Arc<dyn awaken_runtime_host::PluginPublicationResolver>,
) -> Result<Router, String> {
    build_control_assembly_with_model_composition(
        deployment,
        key,
        PublicationModelComposition::HostedPublication { resolver },
        Some((web_search_providers, web_search_publication_resolver)),
    )
    .await
    .map(|assembly| assembly.router)
}

async fn build_control_assembly_with_model_composition(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
    model_composition: PublicationModelComposition,
    web_search: Option<(
        awaken_runtime_host::WebSearchProviderRegistry,
        Arc<dyn awaken_runtime_host::PluginPublicationResolver>,
    )>,
) -> Result<ProcessAssembly, String> {
    let identity = identity_wiring(
        deployment.identity_mode,
        Some(&deployment.data_dir),
        &deployment.org_id,
        &deployment.iam_workspaces,
        &deployment.cloud_iam,
    )?;
    let stores = open_deployment_stores(
        deployment.control.clone(),
        // Control exposes no File/Memory/Skill routes. This volatile
        // resource plane satisfies shared control-plane collaborators without
        // acquiring a second durable resource-plane authority.
        ephemeral_resource_plane(),
        deployment.data_dir.clone(),
        key,
        PostgresSchemaMode::Verify,
    )
    .await?;
    let router = assemble_process_router(
        stores,
        identity.iam,
        identity.remote_iam,
        identity.local_browser_auth,
        model_composition,
        ProcessAssemblyOptions {
            deployment: None,
            org_id: Some(deployment.org_id.clone()),
            mcp_bearer_token: deployment.mcp_bearer_token.clone(),
            role: config::Role::Control,
            cloud_api_base_url: Some(deployment.cloud_iam.inference_base_url.clone()),
            cloud_models_enabled: deployment.cloud_models.is_enabled(),
            local_acp_observations: Vec::new(),
            hand_executors: BTreeMap::new(),
            web_search_providers: web_search.as_ref().map(|value| value.0.clone()),
            web_search_publication_resolver: web_search.map(|value| value.1),
        },
        None,
    )
    .await;
    Ok(ProcessAssembly {
        router,
        local_setup: identity.local_setup,
    })
}
