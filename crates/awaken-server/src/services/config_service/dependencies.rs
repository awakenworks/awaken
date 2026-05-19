use awaken_contract::{AgentSpec, ModelBindingSpec};

use super::normalization::effective_visible_record;
use super::{ConfigNamespace, ConfigService, ConfigServiceError, DependentRef};

impl<'a> ConfigService<'a> {
    /// Return all records in other namespaces that reference `id` in `namespace`.
    ///
    /// - Providers: scans models for `provider_id == id`
    /// - Models: scans agents for `model_id == id`
    /// - Agents / McpServers / Skills: leaf nodes, no dependents
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
                    let Some(model) = effective_visible_record::<ModelBindingSpec>(value)? else {
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
                Ok(refs)
            }
            ConfigNamespace::Agents | ConfigNamespace::McpServers | ConfigNamespace::Skills => {
                Ok(vec![])
            }
        }
    }
}
