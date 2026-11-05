// @formal surface requirement=control.agent_profiles.immutable_profile
// @formal reason: The existing constructor freezes the required publication collaborators and
// @formal reason: catalog-pattern projection before authoring. It owns no credential material,
// @formal reason: scope authorization, execution or second catalog; source-visible constructor and
// @formal reason: publication tests cover the boundary without claiming a formal proof here.
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
        tool_configuration_compiler: Arc<
            dyn awaken_tool_contract::tool_configuration::ToolConfigurationCompiler,
        >,
        plugin_configurations: Vec<
            awaken_executable_agent_contract::capability::PluginConfigurationSource,
        >,
        pattern_selectable: fn(&awaken_runtime_contract::resolved::ToolDescriptor) -> bool,
    ) -> Self {
        Self {
            registrar,
            tool_configuration_compiler,
            pattern_selectable,
            resources: None,
            model_publication_resolver,
            credential_reference_validator: None,
            plugin_publication_resolvers: Vec::new(),
            plugin_configurations,
        }
    }
}
