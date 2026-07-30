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
    fn agent_view_in(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<awaken_session_contract::AgentConfigView> {
        self.agent_view_with_resources(workspace_id, agent_id)
    }

    fn agent_unavailable_in(&self, workspace_id: &str, agent_id: &str) -> bool {
        self.0.agent_unavailable_in(workspace_id, agent_id)
    }
}

impl ConfigServiceAgentSource {
    fn agent_view_with_resources(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<awaken_session_contract::AgentConfigView> {
        let projection = self.0.installed_projection_in(workspace_id, agent_id)?;
        let snapshot = projection.snapshot;
        let spec = &snapshot.resolved_spec;
        let bindings = spec.plugin_config.agent.clone();
        let pinned_defaults_revision = snapshot
            .metadata
            .resolution
            .inputs
            .iter()
            .find(|input| input.kind == "agent_session_defaults" && input.id == agent_id)
            .and_then(|input| match input.version {
                awaken_runtime_contract::ResolvedInputVersion::Revision(revision) => {
                    Some(revision as i64)
                }
                _ => None,
            });
        let current_defaults = self
            .0
            .resources
            .as_ref()
            .and_then(|store| {
                store
                    .get_agent_inputs(workspace_id, agent_id)
                    .ok()
                    .flatten()
            })
            .unwrap_or(awaken_config_resolver::AgentInputConfig {
                agent_id: agent_id.to_string(),
                environment: None,
                inputs: Vec::new(),
                revision: 1,
            });
        let defaults = match pinned_defaults_revision {
            Some(revision) if current_defaults.revision == revision => current_defaults,
            Some(_) => return None,
            None => awaken_config_resolver::AgentInputConfig {
                agent_id: agent_id.to_string(),
                environment: None,
                inputs: Vec::new(),
                revision: 1,
            },
        };
        let managed_model = crate::render_managed_model_id(&projection.authored_model_selection)
            .or_else(|_| {
                crate::render_managed_model_id(&awaken_config_store::ModelSelection::Pinned(
                    spec.model_binding.binding.clone(),
                ))
            })
            .ok();
        Some(awaken_session_contract::AgentConfigView {
            model: managed_model,
            backend_ref: spec.model_binding.backend_ref.clone(),
            system: (!spec.instructions.is_empty()).then(|| spec.instructions.clone()),
            tool_ids: spec
                .tool_descriptors
                .iter()
                .filter(|descriptor| {
                    descriptor.kind != awaken_runtime_contract::resolved::ToolKind::ClientExecuted
                })
                .map(|descriptor| descriptor.id.clone())
                .collect(),
            toolsets: bindings.toolsets.clone(),
            client_tools: spec
                .tool_descriptors
                .iter()
                .filter(|descriptor| {
                    descriptor.kind == awaken_runtime_contract::resolved::ToolKind::ClientExecuted
                })
                .map(|descriptor| awaken_session_contract::AgentClientToolView {
                    name: descriptor.id.clone(),
                    description: descriptor.description.clone(),
                    input_schema: descriptor.parameters.clone(),
                })
                .collect(),
            mcp_servers: bindings
                .mcp_servers
                .into_iter()
                .map(|server| awaken_session_contract::AgentMcpServerView {
                    name: server.name,
                    url: server.url,
                    credential_source_id: server
                        .credential
                        .as_ref()
                        .map(|credential| credential.id.clone()),
                    credential_revision: server
                        .credential
                        .as_ref()
                        .map(|credential| credential.revision),
                })
                .collect(),
            skills: bindings.skills,
            delegate_ids: bindings
                .delegates
                .into_iter()
                .map(|binding| binding.agent_id.0)
                .collect(),
            resources: defaults.inputs,
            environment: defaults.environment.map(|binding| {
                awaken_session_contract::AgentEnvironmentBindingView {
                    environment_id: binding.environment_id,
                    revision: binding.revision,
                }
            }),
        })
    }
}
