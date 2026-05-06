//! Registry trait definitions — lookup interfaces for tools, models, providers, agents, and plugins.

use std::sync::Arc;

#[cfg(feature = "a2a")]
use crate::backend::ExecutionBackendFactory;
use crate::plugins::Plugin;
use awaken_contract::contract::executor::LlmExecutor;
use awaken_contract::contract::tool::Tool;

use awaken_contract::registry_spec::{AgentSpec, ModelBindingSpec};

// ---------------------------------------------------------------------------
// ToolRegistry
// ---------------------------------------------------------------------------

/// Lookup interface for available tools.
pub trait ToolRegistry: Send + Sync {
    /// Get a tool by its ID.
    fn get_tool(&self, id: &str) -> Option<Arc<dyn Tool>>;
    /// List all registered tool IDs.
    fn tool_ids(&self) -> Vec<String>;
}

// ---------------------------------------------------------------------------
// ModelBinding + ModelRegistry
// ---------------------------------------------------------------------------

/// Runtime model binding from a model registry ID to a provider and upstream model.
#[derive(Debug, Clone)]
pub struct ModelBinding {
    /// ProviderRegistry ID.
    pub provider_id: String,
    /// Actual model name sent to the upstream provider.
    pub upstream_model: String,
}

impl From<&ModelBindingSpec> for ModelBinding {
    fn from(spec: &ModelBindingSpec) -> Self {
        Self {
            provider_id: spec.provider_id.clone(),
            upstream_model: spec.upstream_model.clone(),
        }
    }
}

/// Lookup interface for model definitions.
pub trait ModelRegistry: Send + Sync {
    /// Get a model binding by its ID.
    fn get_model(&self, id: &str) -> Option<ModelBinding>;
    /// List all registered model IDs.
    fn model_ids(&self) -> Vec<String>;
}

// ---------------------------------------------------------------------------
// ProviderRegistry
// ---------------------------------------------------------------------------

/// Lookup interface for LLM API client instances.
pub trait ProviderRegistry: Send + Sync {
    /// Get a provider (LLM executor) by its ID.
    fn get_provider(&self, id: &str) -> Option<Arc<dyn LlmExecutor>>;
    /// List all registered provider IDs.
    fn provider_ids(&self) -> Vec<String>;
}

// ---------------------------------------------------------------------------
// AgentSpecRegistry
// ---------------------------------------------------------------------------

/// Lookup interface for serializable agent definitions.
pub trait AgentSpecRegistry: Send + Sync {
    /// Get an agent spec by its ID (returns an owned clone).
    fn get_agent(&self, id: &str) -> Option<AgentSpec>;
    /// List all registered agent IDs.
    fn agent_ids(&self) -> Vec<String>;
}

// ---------------------------------------------------------------------------
// PluginSource
// ---------------------------------------------------------------------------

/// Lookup interface for plugin instances.
///
/// Named `PluginSource` to avoid collision with `crate::plugins::PluginRegistry`
/// (which tracks installed plugin state/keys, not lookup).
pub trait PluginSource: Send + Sync {
    /// Get a plugin by its ID.
    fn get_plugin(&self, id: &str) -> Option<Arc<dyn Plugin>>;
    /// List all registered plugin IDs.
    fn plugin_ids(&self) -> Vec<String>;
}

// ---------------------------------------------------------------------------
// BackendRegistry
// ---------------------------------------------------------------------------

/// Lookup interface for remote delegate backend factories.
#[cfg(feature = "a2a")]
pub trait BackendRegistry: Send + Sync {
    /// Get a backend factory by backend kind.
    fn get_backend_factory(&self, backend: &str) -> Option<Arc<dyn ExecutionBackendFactory>>;
    /// List all registered backend kinds.
    fn backend_ids(&self) -> Vec<String>;
}

// ---------------------------------------------------------------------------
// RegistrySet
// ---------------------------------------------------------------------------

/// Aggregation of all registries passed to the registry resolution pipeline.
#[derive(Clone)]
pub struct RegistrySet {
    pub agents: Arc<dyn AgentSpecRegistry>,
    pub tools: Arc<dyn ToolRegistry>,
    pub models: Arc<dyn ModelRegistry>,
    pub providers: Arc<dyn ProviderRegistry>,
    pub plugins: Arc<dyn PluginSource>,
    #[cfg(feature = "a2a")]
    pub backends: Arc<dyn BackendRegistry>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_binding_spec_converts_to_runtime_model_binding() {
        let spec = ModelBindingSpec {
            id: "default".into(),
            provider_id: "openai".into(),
            upstream_model: "gpt-4o-mini".into(),
        };

        let binding = ModelBinding::from(&spec);

        assert_eq!(binding.provider_id, "openai");
        assert_eq!(binding.upstream_model, "gpt-4o-mini");
    }
}
