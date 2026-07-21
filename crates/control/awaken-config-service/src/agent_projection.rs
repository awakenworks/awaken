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
            .map(|config| {
                config
                    .resources
                    .into_iter()
                    .enumerate()
                    .filter_map(|(index, binding)| {
                        use awaken_config_resolver::ResourceKind as Kind;
                        use awaken_resource_contract::{
                            BindingId, FileId, InputBinding, InputResourceId, MemoryStoreId,
                            RepositoryId,
                        };
                        let target = match binding.kind {
                            Kind::File => InputResourceId::File(FileId::from(binding.resource_id)),
                            Kind::MemoryStore => InputResourceId::MemoryStore(MemoryStoreId::from(
                                binding.resource_id,
                            )),
                            Kind::GithubRepository => {
                                InputResourceId::Repository(RepositoryId::from(binding.resource_id))
                            }
                            // Outputs are an environment capability and Skills are
                            // projected through `skill_ids`; neither belongs in the
                            // File/Memory/Repository input union.
                            Kind::Outputs | Kind::Skill => return None,
                        };
                        Some(InputBinding {
                            binding_id: BindingId::new(format!("agent:{agent_id}:input:{index}")),
                            target,
                            mount_path: binding.mount_path,
                            access: match (binding.kind, binding.access) {
                                (Kind::File, _)
                                | (_, awaken_config_resolver::ResourceAccess::ReadOnly) => {
                                    awaken_resource_contract::ResourceAccess::ReadOnly
                                }
                                (_, awaken_config_resolver::ResourceAccess::ReadWrite) => {
                                    awaken_resource_contract::ResourceAccess::ReadWrite
                                }
                            },
                            instructions: binding.instructions,
                        })
                    })
                    .collect()
            })
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
