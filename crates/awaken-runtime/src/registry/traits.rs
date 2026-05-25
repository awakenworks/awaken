//! Registry trait definitions — lookup interfaces for tools, models, providers, agents, and plugins.

use std::sync::Arc;

#[cfg(feature = "a2a")]
use crate::backend::ExecutionBackendFactory;
use crate::plugins::Plugin;
use awaken_contract::contract::executor::LlmExecutor;
use awaken_contract::contract::tool::Tool;

use crate::registry::model_capabilities::ModelCapabilityPatch;
use awaken_contract::registry_spec::{AgentSpec, ModelPoolSpec, ModelSpec};

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
// ModelRegistry
// ---------------------------------------------------------------------------

/// Lookup interface for model definitions.
///
/// Returns the full [`ModelSpec`] (addressing, capabilities, pricing) so
/// downstream callers — resolvers, cost reporters, context-window policy —
/// can read intrinsic capability fields without a parallel lookup.
pub trait ModelRegistry: Send + Sync {
    /// Get a model spec by its ID.
    fn get_model(&self, id: &str) -> Option<ModelSpec>;
    /// List all registered model IDs.
    fn model_ids(&self) -> Vec<String>;

    /// Get a model **pool** spec by its ID, if the id names a pool.
    ///
    /// Pools share the model id namespace: an `AgentSpec.model_id` resolves to
    /// either a [`ModelSpec`] (single model) or a [`ModelPoolSpec`] (pool).
    /// Registries that do not support pools return `None` (the default).
    fn get_pool(&self, _id: &str) -> Option<ModelPoolSpec> {
        None
    }

    /// List all registered pool IDs. Empty by default.
    fn pool_ids(&self) -> Vec<String> {
        Vec::new()
    }
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
    /// Stable provider definition signature when the registry knows one.
    ///
    /// Pool circuit breaker keys include this value so a provider endpoint or
    /// adapter-options change does not inherit stale health from the previous
    /// definition. Registries without serializable provider specs may return
    /// `None`; the pool resolver still keys by provider id and executor name.
    fn provider_signature(&self, _id: &str) -> Option<String> {
        None
    }

    /// Provider family used for built-in model capability defaults.
    ///
    /// This is commonly the serializable `ProviderSpec.adapter` value. It is
    /// separate from provider id so deployments can name providers
    /// `prod-openai`, `openai-eu`, etc. and still receive OpenAI defaults.
    fn provider_capability_source(&self, _id: &str) -> Option<String> {
        None
    }

    /// Provider-published model capabilities discovered from live metadata.
    ///
    /// The resolver treats this as a trusted provider-discovery overlay before
    /// built-in defaults while still preserving explicit fields in the stored
    /// [`ModelSpec`]. Implementations should expose complete replacement
    /// snapshots for a provider definition rather than silently merging partial
    /// discovery responses.
    fn provider_model_capability(
        &self,
        _provider_id: &str,
        _upstream_model: &str,
    ) -> Option<ModelCapabilityPatch> {
        None
    }
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
