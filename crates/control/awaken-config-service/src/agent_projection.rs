//! Published Agent projection into the neutral Session configuration port.
//!
//! This adapter reads configuration and input-binding repositories only. It does
//! not authenticate callers or evaluate policy; the platform edge supplies the
//! trusted Workspace before invoking the scoped projection.

use std::sync::Arc;

use crate::ConfigService;

/// Adapts the config plane to the Managed Session projection port. Runtime sees
/// the resulting neutral view and never receives the authoring repository itself.
pub struct ConfigServiceAgentSource(pub Arc<ConfigService>);

impl awaken_session_contract::AgentConfigSource for ConfigServiceAgentSource {
    fn agent_view(&self, agent_id: &str) -> Option<awaken_session_contract::AgentConfigView> {
        self.agent_view_with_resources(agent_id, None)
    }

    fn agent_view_in(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<awaken_session_contract::AgentConfigView> {
        self.agent_view_with_resources(agent_id, Some(workspace_id))
    }
}

impl ConfigServiceAgentSource {
    fn agent_view_with_resources(
        &self,
        agent_id: &str,
        workspace_id: Option<&str>,
    ) -> Option<awaken_session_contract::AgentConfigView> {
        let snapshot = self.0.installed(agent_id)?;
        let spec = &snapshot.resolved_spec;
        let bindings = awaken_runtime_contract::agent_bindings::AgentBindings::from_config(
            &spec.plugin_config,
        )
        .unwrap_or_default();
        let resources = self
            .0
            .resources
            .as_ref()
            .and_then(|store| {
                store.get_agent_inputs(
                    workspace_id.unwrap_or(awaken_config_store::DEFAULT_SCOPE),
                    agent_id,
                )
            })
            .map(|config| config.inputs)
            .unwrap_or_default();
        Some(awaken_session_contract::AgentConfigView {
            model: Some(spec.model_binding.model_ref.clone()),
            system: (!spec.instructions.is_empty()).then(|| spec.instructions.clone()),
            tool_ids: spec.tool_descriptors.iter().map(|d| d.id.clone()).collect(),
            mcp_servers: bindings
                .mcp_servers
                .into_iter()
                .map(|server| awaken_session_contract::AgentMcpServerView {
                    name: server.name,
                    url: server.url,
                })
                .collect(),
            skill_ids: bindings.skill_ids,
            resources,
        })
    }
}
