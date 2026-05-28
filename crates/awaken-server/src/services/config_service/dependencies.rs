use awaken_server_contract::{
    AgentSpec, ModelPoolSpec, ModelSpec, SkillSpec, parse_skill_allowed_tool_token,
};
use awaken_tool_pattern::{parse_pattern, pattern_matches};
use serde_json::Value;

use super::normalization::effective_visible_record;
use super::{ConfigNamespace, ConfigService, ConfigServiceError, DependentRef};

impl ConfigService {
    /// Return all records in other namespaces that reference `id` in `namespace`.
    ///
    /// - Providers: scans models for `provider_id == id`
    /// - Models: scans agents for `model_id == id`
    /// - MCP servers: scans skills for explicit `mcp__{server}__...`
    ///   references and matchers that hit currently registered MCP tools
    /// - Agents / Skills: leaf nodes, no dependents
    pub(crate) async fn find_dependents(
        &self,
        namespace: ConfigNamespace,
        id: &str,
    ) -> Result<Vec<DependentRef>, ConfigServiceError> {
        match namespace {
            ConfigNamespace::Providers => {
                let models = self.store.list("models", 0, usize::MAX).await?;
                let mut refs = Vec::new();
                for (model_id, value) in models {
                    let Some(model) = effective_visible_record::<ModelSpec>(value)? else {
                        continue;
                    };
                    if model.provider_id == id {
                        refs.push(DependentRef {
                            namespace: "models",
                            id: model_id,
                        });
                    }
                }
                Ok(refs)
            }
            ConfigNamespace::Models => {
                let agents = self.store.list("agents", 0, usize::MAX).await?;
                let mut refs = Vec::new();
                for (agent_id, value) in agents {
                    let Some(agent) = effective_visible_record::<AgentSpec>(value)? else {
                        continue;
                    };
                    if agent.endpoint.is_none() && agent.model_id == id {
                        refs.push(DependentRef {
                            namespace: "agents",
                            id: agent_id,
                        });
                    }
                }
                // A model may also be a member of a pool; deleting it would
                // break the pool's routing.
                let pools = self.store.list("model-pools", 0, usize::MAX).await?;
                for (pool_id, value) in pools {
                    let Some(pool) = effective_visible_record::<ModelPoolSpec>(value)? else {
                        continue;
                    };
                    if pool.members.iter().any(|member| member.model_id == id) {
                        refs.push(DependentRef {
                            namespace: "model-pools",
                            id: pool_id,
                        });
                    }
                }
                Ok(refs)
            }
            ConfigNamespace::ModelPools => {
                // Agents reference a pool exactly as they reference a model.
                let agents = self.store.list("agents", 0, usize::MAX).await?;
                let mut refs = Vec::new();
                for (agent_id, value) in agents {
                    let Some(agent) = effective_visible_record::<AgentSpec>(value)? else {
                        continue;
                    };
                    if agent.endpoint.is_none() && agent.model_id == id {
                        refs.push(DependentRef {
                            namespace: "agents",
                            id: agent_id,
                        });
                    }
                }
                Ok(refs)
            }
            ConfigNamespace::McpServers => {
                let skills = self.store.list("skills", 0, usize::MAX).await?;
                let prefix = format!("mcp__{id}__");
                let current_mcp_tools = self.current_tool_ids_with_prefix(&prefix);
                let mut refs = Vec::new();
                for (skill_id, value) in skills {
                    let Some(skill) = effective_visible_record::<SkillSpec>(value)? else {
                        continue;
                    };
                    if skill.allowed_tools.iter().any(|token| {
                        skill_allowed_tool_references_mcp_server(token, &prefix, &current_mcp_tools)
                    }) {
                        refs.push(DependentRef {
                            namespace: "skills",
                            id: skill_id,
                        });
                    }
                }
                Ok(refs)
            }
            ConfigNamespace::Agents | ConfigNamespace::Skills => Ok(vec![]),
        }
    }

    fn current_tool_ids_with_prefix(&self, prefix: &str) -> Vec<String> {
        self.state
            .run
            .runtime
            .registry_set()
            .map(|registries| {
                registries
                    .tools
                    .tool_ids()
                    .into_iter()
                    .filter(|tool_id| tool_id.starts_with(prefix))
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn skill_allowed_tool_references_mcp_server(
    token: &str,
    prefix: &str,
    current_mcp_tools: &[String],
) -> bool {
    let Ok(parsed) = parse_skill_allowed_tool_token(token.to_string()) else {
        return false;
    };
    if parsed.scope.is_some() {
        return false;
    }
    if parsed.tool_id.starts_with(prefix) {
        return true;
    }
    if current_mcp_tools.is_empty() {
        return false;
    }
    let Ok(pattern) = parse_pattern(&parsed.tool_id) else {
        return false;
    };
    current_mcp_tools
        .iter()
        .any(|tool_id| pattern_matches(&pattern, tool_id, &Value::Null).is_match())
}
