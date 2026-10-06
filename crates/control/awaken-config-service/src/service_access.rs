//! Construction of the config service. Mutation and publication orchestration
//! stay in `config_plane`; lifecycle references belong to the Resources index.

use std::sync::Arc;

use crate::binding_resolver::ModelPublicationResolver;
use crate::config_service::ConfigService;

impl ConfigService {
    #[must_use]
    pub fn new(
        model_publication_resolver: Arc<dyn ModelPublicationResolver>,
        registrar: Arc<dyn awaken_executable_agent_contract::ExecutableAgentRegistrar>,
    ) -> Self {
        Self {
            registrar,
            resources: None,
            model_publication_resolver,
            credential_reference_validator: None,
            plugin_publication_resolvers: Vec::new(),
        }
    }
}
