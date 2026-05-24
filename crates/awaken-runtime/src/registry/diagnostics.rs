use std::collections::HashSet;
use std::fmt;

use awaken_contract::registry_spec::AgentSpec;
use serde::Serialize;

use super::traits::RegistrySet;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RegistryDiagnostic {
    AgentMissingModel {
        agent_id: String,
        model_id: String,
    },
    ModelMissingProvider {
        model_id: String,
        provider_id: String,
    },
    AgentMissingPlugin {
        agent_id: String,
        plugin_id: String,
    },
    AgentMissingDelegate {
        agent_id: String,
        delegate_id: String,
    },
    AgentHookFilterPluginNotLoaded {
        agent_id: String,
        plugin_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryDiagnosticSeverity {
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RegistryResourceRef {
    pub namespace: &'static str,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SerializableRegistryDiagnostic {
    pub code: &'static str,
    pub severity: RegistryDiagnosticSeverity,
    pub resource: RegistryResourceRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<RegistryResourceRef>,
    pub message: String,
}

impl RegistryDiagnostic {
    pub fn code(&self) -> &'static str {
        match self {
            Self::AgentMissingModel { .. } => "agent_missing_model",
            Self::ModelMissingProvider { .. } => "model_missing_provider",
            Self::AgentMissingPlugin { .. } => "agent_missing_plugin",
            Self::AgentMissingDelegate { .. } => "agent_missing_delegate",
            Self::AgentHookFilterPluginNotLoaded { .. } => "agent_hook_filter_plugin_not_loaded",
        }
    }

    pub fn resource(&self) -> RegistryResourceRef {
        match self {
            Self::AgentMissingModel { agent_id, .. }
            | Self::AgentMissingPlugin { agent_id, .. }
            | Self::AgentMissingDelegate { agent_id, .. }
            | Self::AgentHookFilterPluginNotLoaded { agent_id, .. } => RegistryResourceRef {
                namespace: "agents",
                id: agent_id.clone(),
            },
            Self::ModelMissingProvider { model_id, .. } => RegistryResourceRef {
                namespace: "models",
                id: model_id.clone(),
            },
        }
    }

    pub fn depends_on(&self) -> Option<RegistryResourceRef> {
        match self {
            Self::AgentMissingModel { model_id, .. } => Some(RegistryResourceRef {
                namespace: "models",
                id: model_id.clone(),
            }),
            Self::ModelMissingProvider { provider_id, .. } => Some(RegistryResourceRef {
                namespace: "providers",
                id: provider_id.clone(),
            }),
            Self::AgentMissingPlugin { plugin_id, .. }
            | Self::AgentHookFilterPluginNotLoaded { plugin_id, .. } => Some(RegistryResourceRef {
                namespace: "plugins",
                id: plugin_id.clone(),
            }),
            Self::AgentMissingDelegate { delegate_id, .. } => Some(RegistryResourceRef {
                namespace: "agents",
                id: delegate_id.clone(),
            }),
        }
    }

    pub fn to_serializable(&self) -> SerializableRegistryDiagnostic {
        SerializableRegistryDiagnostic {
            code: self.code(),
            severity: RegistryDiagnosticSeverity::Error,
            resource: self.resource(),
            depends_on: self.depends_on(),
            message: self.to_string(),
        }
    }
}

impl fmt::Display for RegistryDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AgentMissingModel { agent_id, model_id } => {
                write!(
                    f,
                    "agent '{agent_id}' references missing model '{model_id}'"
                )
            }
            Self::ModelMissingProvider {
                model_id,
                provider_id,
            } => write!(
                f,
                "model '{model_id}' references missing provider '{provider_id}'"
            ),
            Self::AgentMissingPlugin {
                agent_id,
                plugin_id,
            } => write!(f, "agent '{agent_id}' uses missing plugin '{plugin_id}'"),
            Self::AgentMissingDelegate {
                agent_id,
                delegate_id,
            } => write!(
                f,
                "agent '{agent_id}' delegates to missing agent '{delegate_id}'"
            ),
            Self::AgentHookFilterPluginNotLoaded {
                agent_id,
                plugin_id,
            } => write!(
                f,
                "agent '{agent_id}' active hook filter references unloaded plugin '{plugin_id}'"
            ),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("registry validation failed: {message}")]
pub struct RegistryValidationError {
    diagnostics: Vec<RegistryDiagnostic>,
    message: String,
}

impl RegistryValidationError {
    pub fn from_diagnostics(diagnostics: Vec<RegistryDiagnostic>) -> Self {
        let diagnostics = dedup_diagnostics(diagnostics);
        let message = diagnostics
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        Self {
            diagnostics,
            message,
        }
    }

    pub fn diagnostics(&self) -> &[RegistryDiagnostic] {
        &self.diagnostics
    }
}

pub fn diagnose_registry_set(registries: &RegistrySet) -> Vec<RegistryDiagnostic> {
    let mut diagnostics = Vec::new();

    for model_id in registries.models.model_ids() {
        let Some(binding) = registries.models.get_model(&model_id) else {
            continue;
        };
        if registries
            .providers
            .get_provider(&binding.provider_id)
            .is_none()
        {
            diagnostics.push(RegistryDiagnostic::ModelMissingProvider {
                model_id,
                provider_id: binding.provider_id,
            });
        }
    }

    for agent_id in registries.agents.agent_ids() {
        let Some(spec) = registries.agents.get_agent(&agent_id) else {
            continue;
        };
        diagnostics.extend(diagnose_agent_spec(registries, &spec));
    }

    diagnostics
}

pub fn diagnose_registry_set_serializable(
    registries: &RegistrySet,
) -> Vec<SerializableRegistryDiagnostic> {
    diagnose_registry_set(registries)
        .into_iter()
        .map(|diagnostic| diagnostic.to_serializable())
        .collect()
}

pub fn validate_registry_set(registries: &RegistrySet) -> Result<(), RegistryValidationError> {
    let diagnostics = diagnose_registry_set(registries);
    if diagnostics.is_empty() {
        Ok(())
    } else {
        Err(RegistryValidationError::from_diagnostics(diagnostics))
    }
}

pub fn diagnose_agent_spec(registries: &RegistrySet, spec: &AgentSpec) -> Vec<RegistryDiagnostic> {
    let mut diagnostics = Vec::new();
    let agent_id = spec.id.clone();

    if spec.endpoint.is_none() {
        match registries.models.get_model(&spec.model_id) {
            Some(binding) => {
                if registries
                    .providers
                    .get_provider(&binding.provider_id)
                    .is_none()
                {
                    diagnostics.push(RegistryDiagnostic::ModelMissingProvider {
                        model_id: spec.model_id.clone(),
                        provider_id: binding.provider_id,
                    });
                }
            }
            None => diagnostics.push(RegistryDiagnostic::AgentMissingModel {
                agent_id: agent_id.clone(),
                model_id: spec.model_id.clone(),
            }),
        }
    }

    for plugin_id in &spec.plugin_ids {
        if registries.plugins.get_plugin(plugin_id).is_none() {
            diagnostics.push(RegistryDiagnostic::AgentMissingPlugin {
                agent_id: agent_id.clone(),
                plugin_id: plugin_id.clone(),
            });
        }
    }

    let loaded_plugins: HashSet<_> = spec.plugin_ids.iter().collect();
    for plugin_id in &spec.active_hook_filter {
        if !loaded_plugins.contains(plugin_id) {
            diagnostics.push(RegistryDiagnostic::AgentHookFilterPluginNotLoaded {
                agent_id: agent_id.clone(),
                plugin_id: plugin_id.clone(),
            });
        }
    }

    let known_agents: HashSet<_> = registries.agents.agent_ids().into_iter().collect();
    for delegate_id in &spec.delegates {
        if !known_agents.contains(delegate_id) {
            diagnostics.push(RegistryDiagnostic::AgentMissingDelegate {
                agent_id: agent_id.clone(),
                delegate_id: delegate_id.clone(),
            });
        }
    }

    diagnostics
}

fn dedup_diagnostics(diagnostics: Vec<RegistryDiagnostic>) -> Vec<RegistryDiagnostic> {
    let mut seen = HashSet::new();
    diagnostics
        .into_iter()
        .filter(|diagnostic| seen.insert(diagnostic.clone()))
        .collect()
}

pub fn validate_agent_spec(
    registries: &RegistrySet,
    spec: &AgentSpec,
) -> Result<(), RegistryValidationError> {
    let diagnostics = diagnose_agent_spec(registries, spec);
    if diagnostics.is_empty() {
        Ok(())
    } else {
        Err(RegistryValidationError::from_diagnostics(diagnostics))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::memory::{
        MapAgentSpecRegistry, MapModelRegistry, MapPluginSource, MapProviderRegistry,
        MapToolRegistry,
    };
    use awaken_contract::registry_spec::ModelSpec;
    use std::sync::Arc;

    fn empty_registry_set() -> RegistrySet {
        RegistrySet {
            agents: Arc::new(MapAgentSpecRegistry::new()),
            tools: Arc::new(MapToolRegistry::new()),
            models: Arc::new(MapModelRegistry::new()),
            providers: Arc::new(MapProviderRegistry::new()),
            plugins: Arc::new(MapPluginSource::new()),
            #[cfg(feature = "a2a")]
            backends: Arc::new(crate::registry::memory::MapBackendRegistry::new()),
        }
    }

    #[test]
    fn diagnose_agent_spec_reports_missing_model() {
        let registries = empty_registry_set();
        let spec = AgentSpec {
            id: "agent".into(),
            model_id: "missing".into(),
            system_prompt: "s".into(),
            ..Default::default()
        };

        let diagnostics = diagnose_agent_spec(&registries, &spec);
        assert_eq!(
            diagnostics,
            vec![RegistryDiagnostic::AgentMissingModel {
                agent_id: "agent".into(),
                model_id: "missing".into(),
            }]
        );
    }

    #[test]
    fn diagnose_registry_set_reports_model_missing_provider() {
        let mut models = MapModelRegistry::new();
        models
            .register_model(ModelSpec::new("m", "missing-provider", "upstream"))
            .unwrap();
        let registries = RegistrySet {
            models: Arc::new(models),
            ..empty_registry_set()
        };

        let diagnostics = diagnose_registry_set(&registries);
        assert_eq!(
            diagnostics,
            vec![RegistryDiagnostic::ModelMissingProvider {
                model_id: "m".into(),
                provider_id: "missing-provider".into(),
            }]
        );
    }

    #[test]
    fn diagnose_registry_set_dedups_model_missing_provider() {
        let mut models = MapModelRegistry::new();
        models
            .register_model(ModelSpec::new("m", "missing-provider", "upstream"))
            .unwrap();
        let mut agents = MapAgentSpecRegistry::new();
        agents
            .register_spec(AgentSpec {
                id: "a".into(),
                model_id: "m".into(),
                system_prompt: "s".into(),
                ..Default::default()
            })
            .unwrap();
        let registries = RegistrySet {
            agents: Arc::new(agents),
            models: Arc::new(models),
            ..empty_registry_set()
        };

        let error = validate_registry_set(&registries).expect_err("registry must be invalid");
        assert_eq!(
            error.diagnostics(),
            &[RegistryDiagnostic::ModelMissingProvider {
                model_id: "m".into(),
                provider_id: "missing-provider".into(),
            }]
        );
    }

    #[test]
    fn diagnose_agent_spec_reports_unloaded_active_hook_filter_plugin() {
        let registries = empty_registry_set();
        let spec = AgentSpec {
            id: "agent".into(),
            model_id: "m".into(),
            system_prompt: "s".into(),
            active_hook_filter: ["missing-plugin".to_string()].into_iter().collect(),
            ..Default::default()
        };

        let diagnostics = diagnose_agent_spec(&registries, &spec);
        assert!(
            diagnostics.contains(&RegistryDiagnostic::AgentHookFilterPluginNotLoaded {
                agent_id: "agent".into(),
                plugin_id: "missing-plugin".into(),
            })
        );
    }

    #[test]
    fn serializable_diagnostic_has_stable_code_resource_and_dependency() {
        let diagnostic = RegistryDiagnostic::ModelMissingProvider {
            model_id: "m".into(),
            provider_id: "p".into(),
        }
        .to_serializable();

        assert_eq!(diagnostic.code, "model_missing_provider");
        assert_eq!(diagnostic.severity, RegistryDiagnosticSeverity::Error);
        assert_eq!(diagnostic.resource.namespace, "models");
        assert_eq!(diagnostic.resource.id, "m");
        assert_eq!(
            diagnostic.depends_on,
            Some(RegistryResourceRef {
                namespace: "providers",
                id: "p".into(),
            })
        );
        assert!(diagnostic.message.contains("missing provider"));
    }
}
