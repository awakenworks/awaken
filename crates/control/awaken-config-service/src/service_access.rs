//! Construction and read-only projections of the config service. Mutation and
//! publication orchestration stay in `config_plane`.

use std::sync::Arc;

use awaken_runtime_contract::ExecutableAgentSnapshot;

use crate::binding_resolver::ModelPublicationResolver;
use crate::config_plane::ConfigService;
use crate::installed_catalog::InstalledAgentCatalog;

impl ConfigService {
    #[must_use]
    pub fn new(model_publication_resolver: Arc<dyn ModelPublicationResolver>) -> Self {
        Self {
            installed: InstalledAgentCatalog::default(),
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

    pub fn agents_referencing_skill(&self, workspace_id: &str, skill_id: &str) -> Vec<String> {
        self.installed
            .agents_referencing_skill(workspace_id, skill_id)
    }

    pub fn installed_in(&self, workspace: &str, agent: &str) -> Option<ExecutableAgentSnapshot> {
        self.installed.snapshot_in(workspace, agent)
    }

    #[must_use]
    pub fn agent_unavailable_in(&self, workspace: &str, agent: &str) -> bool {
        self.installed.is_unavailable(workspace, agent)
    }

    /// Current logical Hand declaration for a globally unambiguous Agent id.
    ///
    /// Placement is deliberately absent from executable snapshots. A collision
    /// across Workspaces fails closed rather than selecting one deployment.
    pub fn declared_hand_for_agent(&self, agent: &str) -> Result<Option<String>, String> {
        self.installed.declared_hand_for_agent(agent)
    }
}
