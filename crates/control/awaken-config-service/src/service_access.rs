//! Construction and read-only projections of the config service. Mutation and
//! publication orchestration stay in `config_plane`.

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

    /// Agent ids whose current default-input configuration references `target` in
    /// one Workspace. This is resource lifecycle evidence, not authorization.
    pub fn agents_referencing_input(
        &self,
        workspace_id: &str,
        target: &awaken_config_resolver::InputResourceId,
    ) -> Vec<String> {
        self.resources
            .as_ref()
            .map(|store| {
                store
                    .list_agent_inputs(workspace_id)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|config| config.inputs.iter().any(|input| &input.target == target))
                    .map(|config| config.agent_id)
                    .collect()
            })
            .unwrap_or_default()
    }
}
