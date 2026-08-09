//! Typed optional collaborators for the one process assembly path.

use std::sync::Arc;

use super::{config, executable_agent_registration, executable_environment_registration};

#[derive(Default)]
pub(super) struct ProcessAssemblyOptions {
    pub(super) deployment: Option<awaken_runtime_host::DeploymentConfig>,
    pub(super) content_capture_ceiling: awaken_runtime_contract::ContentCapture,
    pub(super) org_id: Option<String>,
    pub(super) mcp_bearer_token: Option<String>,
    pub(super) role: config::Role,
    pub(super) cloud_api_base_url: Option<String>,
    pub(super) model_supply: awaken_admin_config_api::ModelSupplyCapabilityView,
    pub(super) brokered_catalog: Option<Arc<dyn awaken_admin_config_api::BrokeredCatalogDiscovery>>,
    pub(super) local_acp_observations: Vec<awaken_acp_application::AcpHostObservation>,
    pub(super) web_search_providers: Option<awaken_ext_builtin_tools::WebSearchProviderRegistry>,
    pub(super) web_search_publication_resolver:
        Option<Arc<dyn awaken_config_service::PluginPublicationResolver>>,
    pub(super) executable_agent_wiring:
        Option<executable_agent_registration::ExecutableAgentWiring>,
    pub(super) executable_environment_wiring:
        Option<executable_environment_registration::ExecutableEnvironmentWiring>,
    pub(super) worker_authenticator:
        Option<Arc<dyn awaken_worker_transport_security::WorkerRequestAuthenticator>>,
    pub(super) control_service_token: Option<String>,
    pub(super) control_service: Option<super::ControlServicePorts>,
}

pub(super) fn local_model_supply(
    cloud_models_enabled: bool,
) -> awaken_admin_config_api::ModelSupplyCapabilityView {
    awaken_admin_config_api::ModelSupplyCapabilityView {
        cloud_models_enabled,
        ..Default::default()
    }
}
