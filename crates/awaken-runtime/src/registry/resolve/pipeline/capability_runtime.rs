use awaken_runtime_contract::registry_spec::{AgentSpec, ModelSpec};

use crate::registry::model_capabilities::ModelCapabilitySources;

use super::ResolveError;

pub(super) fn knowledge_cutoff_context(
    spec: &AgentSpec,
    model: &ModelSpec,
    capability_sources: &ModelCapabilitySources,
) -> Result<Option<String>, ResolveError> {
    let config = spec
        .config::<crate::context::KnowledgeCutoffConfigKey>()
        .map_err(|error| match error {
            awaken_runtime_contract::StateError::KeyDecode { key, message } => {
                ResolveError::InvalidPluginConfig {
                    plugin: crate::context::KNOWLEDGE_CUTOFF_PLUGIN_ID.into(),
                    key,
                    message,
                }
            }
            other => ResolveError::EnvBuild(other),
        })?;
    if !config.enabled {
        return Ok(None);
    }
    if !capability_sources
        .knowledge_cutoff
        .is_some_and(|source| source.is_runtime_trusted())
    {
        return Ok(None);
    }
    Ok(model.knowledge_cutoff.clone())
}
