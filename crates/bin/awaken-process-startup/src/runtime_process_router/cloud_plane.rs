// @formal surface requirement=assembly.binary_roles.role_assembly
// @formal reason: The runtime process composition selects the brokered Cloud client, the local Vault
// @formal reason: credential materializer and the model publication wiring once per role; moved
// @formal reason: unchanged out of prepare_runtime_routers. Classification, not a formal-proof claim.
//! The cloud, credential and model plane of the runtime process: brokered
//! cloud inference, credential materialization, Web search and model
//! publication.

use std::sync::Arc;

use super::ProcessBase;
use crate::config;
use crate::hosted_model_publication::{
    PublicationModelSupply, brokered_inference_client, local_client_instance_id,
};
use crate::live_runtime_capabilities::LiveRuntimeCapabilities;
use crate::{
    ResolvedModelServices, RuntimeModelWiring, resolve_model_services, runtime_model_wiring,
};

/// Brokered cloud inference, credential materialization, Web search and model
/// publication: everything a model call or a model publication needs.
pub(super) struct CloudPlane {
    pub(super) brokered_client:
        Option<Arc<awaken_inference_endpoint::brokered_inference::HttpBrokeredInferenceClient>>,
    pub(super) credential_materializer:
        Option<awaken_credential_materializer::PinnedCredentialMaterializer>,
    pub(super) credential_refresh_factory:
        Option<Arc<dyn awaken_credential_contract::CredentialRefreshFactory>>,
    pub(super) attempt_credential_deliveries:
        Option<Arc<dyn awaken_credential_contract::AttemptCredentialDeliverySource>>,
    pub(super) web_search_providers: awaken_ext_builtin_tools::WebSearchProviderRegistry,
    pub(super) web_search_publication_resolver:
        Option<Arc<dyn awaken_config_service::PluginPublicationResolver>>,
    pub(super) model_services: Option<ResolvedModelServices>,
    pub(super) model_wiring: RuntimeModelWiring,
    pub(super) live_runtime_capabilities: Option<Arc<LiveRuntimeCapabilities>>,
}

pub(super) async fn assemble_cloud_plane(
    base: &mut ProcessBase,
    model_supply: PublicationModelSupply,
) -> Result<CloudPlane, String> {
    let brokered_client = brokered_cloud_client(base).await?;
    let credentials = credential_plane(base);
    let models = model_plane(
        base,
        model_supply,
        brokered_client.as_ref(),
        credentials.materializer.as_ref(),
    )
    .await;
    Ok(CloudPlane {
        brokered_client,
        credential_materializer: credentials.materializer,
        credential_refresh_factory: credentials.refresh_factory,
        attempt_credential_deliveries: credentials.attempt_deliveries,
        web_search_providers: models.web_search_providers,
        web_search_publication_resolver: models.web_search_publication_resolver,
        model_services: models.model_services,
        model_wiring: models.model_wiring,
        live_runtime_capabilities: models.live_runtime_capabilities,
    })
}

async fn brokered_cloud_client(
    base: &ProcessBase,
) -> Result<
    Option<Arc<awaken_inference_endpoint::brokered_inference::HttpBrokeredInferenceClient>>,
    String,
> {
    let stores = &base.stores;
    // Store startup resolves the installation Workspace exactly once after its
    // canonical Session authority succeeds. Every adapter receives that value;
    // this router never publishes or regenerates installation identity.
    let brokered_client = brokered_inference_client(
        base.process.model_supply.cloud_models_enabled && base.role == config::Role::AllInOne,
        base.remote_iam.as_ref(),
        base.process.cloud_developer_key_file.as_deref(),
        base.process.cloud_api_base_url.as_deref(),
        &stores.platform_workspace,
        &local_client_instance_id(stores.workspace_root.as_deref()),
    );
    // Cloud-catalog startup decision table:
    // | Cloud models | authenticated brokered client | effect |
    // | disabled | absent | retain local/BYOK catalog; zero Cloud reads |
    // | enabled | present | reconcile once into the authoritative catalog |
    // | enabled | construction/admission failure | fail startup closed |
    // Explicit refresh remains the same ConfigService command; this adds no
    // timer, second catalog, or alternate discovery path.
    if let Some(client) = brokered_client.as_ref() {
        let catalog = stores
            .control
            .as_ref()
            .expect("AllInOne Cloud model discovery owns Control stores")
            .catalog
            .clone();
        let discovery = crate::brokered_catalog::BrokeredCatalogAdapter::new(
            client.clone(),
            stores.clock.clone(),
        );
        awaken_admin_config_api::reconcile_brokered_catalog(&discovery, catalog.as_ref())
            .await
            .map_err(|error| {
                format!("initial Cloud model catalog reconciliation failed: {error}")
            })?;
    }
    Ok(brokered_client)
}

struct CredentialPlane {
    materializer: Option<awaken_credential_materializer::PinnedCredentialMaterializer>,
    refresh_factory: Option<Arc<dyn awaken_credential_contract::CredentialRefreshFactory>>,
    attempt_deliveries:
        Option<Arc<dyn awaken_credential_contract::AttemptCredentialDeliverySource>>,
}

fn credential_plane(base: &mut ProcessBase) -> CredentialPlane {
    let stores = &base.stores;
    let role = base.role;
    let local_credential_authority = (role == config::Role::AllInOne)
        .then(|| {
            stores.control.as_ref().map(|control| {
                Arc::new(awaken_credential_vault::VaultLocalMaterialAuthority::new(
                    control.credentials.clone(),
                    control.secrets.clone(),
                    stores.clock.clone(),
                ))
            })
        })
        .flatten();
    let mut materializer = local_credential_authority.as_ref().map(|authority| {
        awaken_credential_materializer::PinnedCredentialMaterializer::new(
            authority.clone(),
            stores.clock.clone(),
        )
    });
    if let (Some(materializer), Some(resolver)) = (
        materializer.as_mut(),
        base.process.credential_material_resolver.take(),
    ) {
        *materializer = materializer
            .clone()
            .with_external_material_resolver(resolver);
    }
    let refresh_factory = (role == config::Role::AllInOne)
        .then(|| {
            stores.control.as_ref().map(|control| {
                Arc::new(awaken_control::VaultRefreshFactory::new(
                    control.credentials.clone(),
                    control.secrets.clone(),
                    stores.clock.clone(),
                )) as Arc<dyn awaken_credential_contract::CredentialRefreshFactory>
            })
        })
        .flatten();
    let attempt_deliveries = match (
        local_credential_authority,
        base.process
            .managed_services
            .credential_material_delivery
            .clone(),
    ) {
        (
            Some(authority),
            Some(awaken_credential_contract::CredentialMaterialDelivery::RecipientEnvelope(issuer)),
        ) => Some(Arc::new(
            awaken_credential_vault::VaultAttemptCredentialDeliverySource::new(authority, issuer),
        )
            as Arc<
                dyn awaken_credential_contract::AttemptCredentialDeliverySource,
            >),
        _ => None,
    };
    CredentialPlane {
        materializer,
        refresh_factory,
        attempt_deliveries,
    }
}

struct ModelPlane {
    web_search_providers: awaken_ext_builtin_tools::WebSearchProviderRegistry,
    web_search_publication_resolver:
        Option<Arc<dyn awaken_config_service::PluginPublicationResolver>>,
    model_services: Option<ResolvedModelServices>,
    model_wiring: RuntimeModelWiring,
    live_runtime_capabilities: Option<Arc<LiveRuntimeCapabilities>>,
}

async fn model_plane(
    base: &mut ProcessBase,
    model_supply: PublicationModelSupply,
    brokered_client: Option<
        &Arc<awaken_inference_endpoint::brokered_inference::HttpBrokeredInferenceClient>,
    >,
    credential_materializer: Option<&awaken_credential_materializer::PinnedCredentialMaterializer>,
) -> ModelPlane {
    let stores = &base.stores;
    let role = base.role;
    let cloud_models_enabled = base.process.model_supply.cloud_models_enabled;
    let worker_observations = base.worker_observations.source.clone();
    let mut web_search_providers = base
        .process
        .web_search_providers
        .take()
        .unwrap_or_else(awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins);
    if let Some(client) = brokered_client
        && let Err(error) = client
            .install_managed_web_routes(&mut web_search_providers)
            .await
    {
        eprintln!("Awaken Cloud Web tools are unavailable: {error}");
    }
    let model_services = (role == config::Role::AllInOne).then(|| {
        resolve_model_services(
            model_supply,
            stores,
            cloud_models_enabled,
            base.process
                .managed_services
                .credential_material_delivery
                .clone(),
            worker_observations.clone(),
        )
    });
    let mut model_wiring = match (&model_services, credential_materializer) {
        (Some(process), Some(credentials)) => runtime_model_wiring(
            process.runtime.clone(),
            credentials,
            cloud_models_enabled,
            brokered_client,
        ),
        (None, None) => RuntimeModelWiring {
            executor: Arc::new(awaken_session_executor::NoModelConfiguredExecutor),
            model_ref: awaken_session_executor::UNCONFIGURED_MODEL_REF.to_string(),
            materializer: None,
        },
        _ => unreachable!("Control model adapters and Control stores are configured together"),
    };
    if let Some(materializer) = base.process.inference_materializer.take() {
        model_wiring.materializer = Some(materializer);
    }
    let web_search_publication_resolver = base
        .process
        .web_search_publication_resolver
        .take()
        .or_else(|| {
            stores.control.as_ref().map(|control| {
                Arc::new(
                    crate::web_search_publication::WebSearchPublicationResolver::new(
                        web_search_providers.clone(),
                        control.credentials.clone(),
                        stores.clock.clone(),
                    ),
                ) as Arc<dyn awaken_config_service::PluginPublicationResolver>
            })
        });
    let live_runtime_capabilities = stores.control.as_ref().map(|control| {
        Arc::new(LiveRuntimeCapabilities {
            initial: base.process.local_acp_observations.clone(),
            workers: worker_observations.clone(),
            credentials: control.credentials.clone(),
            workspace: stores.platform_workspace.clone(),
            clock: stores.clock.clone(),
        })
    });
    ModelPlane {
        web_search_providers,
        web_search_publication_resolver,
        model_services,
        model_wiring,
        live_runtime_capabilities,
    }
}
