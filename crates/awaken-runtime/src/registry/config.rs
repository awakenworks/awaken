//! JSON config file loading for agent system configuration.
//!
//! Parses `AgentSystemConfig` from JSON to populate model and agent registries.
//! Providers (trait objects) are passed in programmatically — they are not serializable.
//! See ADR-0010 D7.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use awaken_contract::contract::executor::LlmExecutor;
use awaken_contract::registry_spec::{AgentSpec, ModelBindingSpec};

use crate::builder::BuildError;

#[cfg(feature = "a2a")]
use super::BackendRegistry;
#[cfg(feature = "a2a")]
use super::memory::MapBackendRegistry;
use super::memory::{
    MapAgentSpecRegistry, MapModelRegistry, MapPluginSource, MapProviderRegistry, MapToolRegistry,
};
use super::traits::{ModelBinding, RegistrySet};

/// Serializable system configuration covering models and agents.
///
/// Providers are not included because they hold trait objects (`Arc<dyn LlmExecutor>`)
/// that cannot be deserialized. Pass them to [`AgentSystemConfig::build_registries`] instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSystemConfig {
    /// Model bindings.
    #[serde(default)]
    pub models: Vec<ModelBindingSpec>,
    /// Agent definitions.
    #[serde(default)]
    pub agents: Vec<AgentSpec>,
}

impl AgentSystemConfig {
    /// Deserialize from a JSON string.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Build a [`RegistrySet`] from this config plus externally-supplied providers.
    ///
    /// Tools and plugins are empty — they are registered programmatically.
    pub fn build_registries(
        &self,
        providers: HashMap<String, Arc<dyn LlmExecutor>>,
    ) -> Result<RegistrySet, BuildError> {
        let mut model_reg = MapModelRegistry::new();
        for binding in &self.models {
            model_reg.register_model(binding.id.clone(), ModelBinding::from(binding))?;
        }

        let mut agent_reg = MapAgentSpecRegistry::new();
        for spec in &self.agents {
            agent_reg.register_spec(spec.clone())?;
        }

        let mut provider_reg = MapProviderRegistry::new();
        for (id, executor) in providers {
            provider_reg.register_provider(id, executor)?;
        }

        Ok(RegistrySet {
            agents: Arc::new(agent_reg),
            tools: Arc::new(MapToolRegistry::new()),
            models: Arc::new(model_reg),
            providers: Arc::new(provider_reg),
            plugins: Arc::new(MapPluginSource::new()),
            #[cfg(feature = "a2a")]
            backends: Arc::new(MapBackendRegistry::with_default_remote_backends())
                as Arc<dyn BackendRegistry>,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_minimal_config() {
        let json = json!({
            "models": [
                {
                    "id": "gpt4",
                    "provider_id": "openai",
                    "upstream_model": "gpt-4o"
                }
            ],
            "agents": [{
                "id": "assistant",
                "model_id": "gpt4",
                "system_prompt": "You are helpful."
            }]
        });

        let config = AgentSystemConfig::from_json(&json.to_string()).unwrap();
        assert_eq!(config.models.len(), 1);
        assert_eq!(config.models[0].provider_id, "openai");
        assert_eq!(config.models[0].upstream_model, "gpt-4o");
        assert_eq!(config.agents.len(), 1);
        assert_eq!(config.agents[0].id, "assistant");
        assert_eq!(config.agents[0].model_id, "gpt4");
    }

    #[test]
    fn parse_multiple_agents() {
        let json = json!({
            "models": [
                { "id": "claude", "provider_id": "anthropic", "upstream_model": "claude-opus-4-0-20250514" },
                { "id": "local", "provider_id": "ollama", "upstream_model": "llama3" }
            ],
            "agents": [
                {
                    "id": "coder",
                    "model_id": "claude",
                    "system_prompt": "You write code.",
                    "allowed_tools": ["read_file", "write_file"],
                    "excluded_tools": ["delete_file"]
                },
                {
                    "id": "reviewer",
                    "model_id": "local",
                    "system_prompt": "You review code.",
                    "allowed_tools": ["read_file"]
                }
            ]
        });

        let config = AgentSystemConfig::from_json(&json.to_string()).unwrap();
        assert_eq!(config.agents.len(), 2);

        let coder = &config.agents[0];
        assert_eq!(coder.id, "coder");
        assert_eq!(
            coder.allowed_tools,
            Some(vec!["read_file".to_string(), "write_file".to_string()])
        );
        assert_eq!(coder.excluded_tools, Some(vec!["delete_file".to_string()]));

        let reviewer = &config.agents[1];
        assert_eq!(reviewer.id, "reviewer");
        assert_eq!(reviewer.model_id, "local");
        assert_eq!(reviewer.allowed_tools, Some(vec!["read_file".to_string()]));
        assert!(reviewer.excluded_tools.is_none());
    }

    #[test]
    fn build_registries_from_config() {
        use async_trait::async_trait;
        use awaken_contract::contract::executor::{
            InferenceExecutionError, InferenceRequest, LlmExecutor,
        };
        use awaken_contract::contract::inference::StreamResult;
        use std::sync::Arc;

        struct StubExecutor;

        #[async_trait]
        impl LlmExecutor for StubExecutor {
            async fn execute(
                &self,
                _request: InferenceRequest,
            ) -> Result<StreamResult, InferenceExecutionError> {
                unimplemented!()
            }

            fn name(&self) -> &str {
                "stub"
            }
        }

        let config = AgentSystemConfig::from_json(
            &json!({
                "models": [
                    { "id": "m1", "provider_id": "stub", "upstream_model": "test-model" }
                ],
                "agents": [{
                    "id": "a1",
                    "model_id": "m1",
                    "system_prompt": "test"
                }]
            })
            .to_string(),
        )
        .unwrap();

        let mut providers = HashMap::new();
        providers.insert(
            "stub".to_string(),
            Arc::new(StubExecutor) as Arc<dyn LlmExecutor>,
        );

        let reg = config.build_registries(providers).unwrap();

        // Verify model registry
        let model = reg.models.get_model("m1").unwrap();
        assert_eq!(model.provider_id, "stub");
        assert_eq!(model.upstream_model, "test-model");

        // Verify agent registry
        let agent = reg.agents.get_agent("a1").unwrap();
        assert_eq!(agent.system_prompt, "test");

        // Verify provider registry
        assert!(reg.providers.get_provider("stub").is_some());

        // Tools and plugins are empty
        assert!(reg.tools.tool_ids().is_empty());
    }

    #[test]
    fn config_serde_roundtrip() {
        let original = AgentSystemConfig {
            models: vec![ModelBindingSpec {
                id: "opus".to_string(),
                provider_id: "anthropic".to_string(),
                upstream_model: "claude-opus-4-0-20250514".to_string(),
            }],
            agents: vec![AgentSpec {
                id: "coder".into(),
                model_id: "opus".into(),
                system_prompt: "Code assistant.".into(),
                max_rounds: 10,
                plugin_ids: vec!["logging".into()],
                allowed_tools: Some(vec!["read".into()]),
                excluded_tools: None,
                ..Default::default()
            }],
        };

        let json_str = serde_json::to_string(&original).unwrap();
        let restored: AgentSystemConfig = serde_json::from_str(&json_str).unwrap();

        assert_eq!(restored.models.len(), 1);
        assert_eq!(restored.models[0].id, "opus");
        assert_eq!(restored.models[0].provider_id, "anthropic");
        assert_eq!(
            restored.models[0].upstream_model,
            "claude-opus-4-0-20250514"
        );
        assert_eq!(restored.agents.len(), 1);
        assert_eq!(restored.agents[0].id, "coder");
        assert_eq!(restored.agents[0].max_rounds, 10);
        assert_eq!(restored.agents[0].plugin_ids, vec!["logging"]);
        assert_eq!(
            restored.agents[0].allowed_tools,
            Some(vec!["read".to_string()])
        );
    }
}
