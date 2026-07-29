//! Hosted authoring/control composition over the canonical Management stores.
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
pub async fn build_control_router_with_deployment(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
) -> Result<Router, String> {
    build_control_assembly_with_deployment(deployment, key)
        .await
        .map(|assembly| assembly.router)
}

/// Canonical hosted assembly, including the one-time local setup handoff owned
/// by the shared identity wiring. The router-only entry point projects this
/// value instead of maintaining a second composition path.
pub async fn build_control_assembly_with_deployment(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
) -> Result<ManagementAssembly, String> {
    build_control_assembly_with_model_composition(
        deployment,
        key,
        ManagementModelComposition::PublishedProviders,
    )
    .await
}

/// Canonical hosted control assembly with a deployment-owned provider
/// publication resolver.
///
/// Awaken continues to own Agent authoring, compilation, fingerprinting, and
/// publication persistence. A closed deployment supplies only the existing
/// [`ModelPublicationResolver`](awaken_runtime_host::ModelPublicationResolver)
/// port; it does not replace the router, config service, or publication store.
pub async fn build_control_router_with_publication_resolver(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
    resolver: Arc<dyn awaken_runtime_host::ModelPublicationResolver>,
) -> Result<Router, String> {
    build_control_assembly_with_model_composition(
        deployment,
        key,
        ManagementModelComposition::HostedPublication { resolver },
    )
    .await
    .map(|assembly| assembly.router)
}

async fn build_control_assembly_with_model_composition(
    deployment: &config::ResolvedDeployment,
    key: &[u8; 32],
    model_composition: ManagementModelComposition,
) -> Result<ManagementAssembly, String> {
    let identity = identity_wiring(
        deployment.identity_mode,
        Some(&deployment.data_dir),
        &deployment.org_id,
        &deployment.iam_workspaces,
        &deployment.cloud_iam,
    )?;
    let stores = open_management_stores(
        deployment.control.clone(),
        // Hosted Management exposes no File/Memory/Skill routes. These ports
        // satisfy shared control-plane collaborators without acquiring a second
        // durable resource-plane authority.
        ResourcePlaneStores::ephemeral(),
        deployment.data_dir.clone(),
        key,
        PostgresSchemaMode::Verify,
    )
    .await?;
    let router = management_router_over(
        stores,
        identity.iam,
        identity.remote_iam,
        identity.local_browser_auth,
        model_composition,
        AssemblyOverrides {
            deployment: None,
            org_id: Some(deployment.org_id.clone()),
            mcp_bearer_token: deployment.mcp_bearer_token.clone(),
            management_only: true,
            cloud_api_base_url: Some(deployment.cloud_iam.inference_base_url.clone()),
            cloud_models_enabled: deployment.cloud_models.is_enabled(),
            local_acp_observations: Vec::new(),
            hand_executors: BTreeMap::new(),
        },
        None,
    )
    .await;
    Ok(ManagementAssembly {
        router,
        local_setup: identity.local_setup,
    })
}
