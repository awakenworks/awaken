//! JSON config file loading for agent system configuration.
//!
//! Parses `AgentSystemConfig` from JSON to populate model and agent registries.
//! Providers (trait objects) are passed in programmatically — they are not serializable.
//! See ADR-0010 D7.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::contract::executor::LlmExecutor;

use super::memory::{
    MapAgentSpecRegistry, MapModelRegistry, MapPluginSource, MapProviderRegistry, MapToolRegistry,
};
use super::traits::{ModelEntry, RegistrySet};

/// Serializable system configuration covering models and agents.
///
/// Providers are not included because they hold trait objects (`Arc<dyn LlmExecutor>`)
/// that cannot be deserialized. Pass them to [`AgentSystemConfig::build_registries`] instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSystemConfig {
    /// Model definitions keyed by model ID.
    #[serde(default)]
    pub models: HashMap<String, ModelConfig>,
    /// Agent definitions.
    #[serde(default)]
    pub agents: Vec<super::spec::AgentSpec>,
}

/// Maps a model ID to a provider and the actual model name for API calls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Provider ID — must match a key in the `providers` map passed to `build_registries`.
    pub provider: String,
    /// Actual model name sent to the LLM API.
    pub model: String,
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
    ) -> RegistrySet {
        let mut model_reg = MapModelRegistry::new();
        for (id, cfg) in &self.models {
            model_reg.register(
                id.clone(),
                ModelEntry {
                    provider: cfg.provider.clone(),
                    model_name: cfg.model.clone(),
                },
            );
        }

        let mut agent_reg = MapAgentSpecRegistry::new();
        for spec in &self.agents {
            agent_reg.register(spec.clone());
        }

        let mut provider_reg = MapProviderRegistry::new();
        for (id, executor) in providers {
            provider_reg.register(id, executor);
        }

        RegistrySet {
            agents: Arc::new(agent_reg),
            tools: Arc::new(MapToolRegistry::new()),
            models: Arc::new(model_reg),
            providers: Arc::new(provider_reg),
            plugins: Arc::new(MapPluginSource::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::spec::AgentSpec;
    use serde_json::json;

    #[test]
    fn parse_minimal_config() {
        let json = json!({
            "models": {
                "gpt4": {
                    "provider": "openai",
                    "model": "gpt-4o"
                }
            },
            "agents": [{
                "id": "assistant",
                "model": "gpt4",
                "system_prompt": "You are helpful."
            }]
        });

        let config = AgentSystemConfig::from_json(&json.to_string()).unwrap();
        assert_eq!(config.models.len(), 1);
        assert_eq!(config.models["gpt4"].provider, "openai");
        assert_eq!(config.models["gpt4"].model, "gpt-4o");
        assert_eq!(config.agents.len(), 1);
        assert_eq!(config.agents[0].id, "assistant");
        assert_eq!(config.agents[0].model, "gpt4");
    }

    #[test]
    fn parse_multiple_agents() {
        let json = json!({
            "models": {
                "claude": { "provider": "anthropic", "model": "claude-opus-4-0-20250514" },
                "local": { "provider": "ollama", "model": "llama3" }
            },
            "agents": [
                {
                    "id": "coder",
                    "model": "claude",
                    "system_prompt": "You write code.",
                    "allowed_tools": ["read_file", "write_file"],
                    "excluded_tools": ["delete_file"]
                },
                {
                    "id": "reviewer",
                    "model": "local",
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
        assert_eq!(reviewer.model, "local");
        assert_eq!(reviewer.allowed_tools, Some(vec!["read_file".to_string()]));
        assert!(reviewer.excluded_tools.is_none());
    }

    #[test]
    fn build_registries_from_config() {
        use crate::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
        use crate::contract::inference::StreamResult;
        use async_trait::async_trait;
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
                "models": {
                    "m1": { "provider": "stub", "model": "test-model" }
                },
                "agents": [{
                    "id": "a1",
                    "model": "m1",
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

        let reg = config.build_registries(providers);

        // Verify model registry
        let model = reg.models.get_model("m1").unwrap();
        assert_eq!(model.provider, "stub");
        assert_eq!(model.model_name, "test-model");

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
            models: {
                let mut m = HashMap::new();
                m.insert(
                    "opus".to_string(),
                    ModelConfig {
                        provider: "anthropic".to_string(),
                        model: "claude-opus-4-0-20250514".to_string(),
                    },
                );
                m
            },
            agents: vec![AgentSpec {
                id: "coder".into(),
                model: "opus".into(),
                system_prompt: "Code assistant.".into(),
                max_rounds: 10,
                plugin_ids: vec!["logging".into()],
                allowed_tools: Some(vec!["read".into()]),
                excluded_tools: None,
                sections: Default::default(),
            }],
        };

        let json_str = serde_json::to_string(&original).unwrap();
        let restored: AgentSystemConfig = serde_json::from_str(&json_str).unwrap();

        assert_eq!(restored.models.len(), 1);
        assert_eq!(restored.models["opus"].provider, "anthropic");
        assert_eq!(restored.models["opus"].model, "claude-opus-4-0-20250514");
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
