//! In-memory HashMap-backed registry implementations.

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

#[cfg(feature = "a2a")]
use crate::backend::ExecutionBackendFactory;
use crate::builder::BuildError;
#[cfg(feature = "a2a")]
use crate::extensions::a2a::A2aBackendFactory;
use crate::plugins::Plugin;
use awaken_contract::contract::executor::LlmExecutor;
use awaken_contract::contract::tool::Tool;

#[cfg(feature = "a2a")]
use super::traits::BackendRegistry;
use super::traits::{
    AgentSpecRegistry, ModelRegistry, PluginSource, ProviderRegistry, ToolRegistry,
};
use awaken_contract::registry_spec::{AgentSpec, ModelPoolSpec, ModelSpec};
use awaken_contract::{validate_model_pool_spec_struct, validate_model_spec_struct};

// ---------------------------------------------------------------------------
// MapRegistry<V> — generic in-memory registry
// ---------------------------------------------------------------------------

/// In-memory registry backed by a `HashMap`.
///
/// All five concrete registry types (`MapToolRegistry`, `MapModelRegistry`,
/// `MapProviderRegistry`, `MapAgentSpecRegistry`, `MapPluginSource`) are type
/// aliases over this single generic struct.
#[derive(Default)]
pub struct MapRegistry<V> {
    items: HashMap<String, V>,
}

impl<V> MapRegistry<V> {
    pub fn new() -> Self {
        Self {
            items: HashMap::new(),
        }
    }

    /// Register a value under `id`, returning an error (via `make_err`) on
    /// duplicate keys.
    pub fn register(
        &mut self,
        id: impl Into<String>,
        value: V,
        make_err: impl FnOnce(String) -> BuildError,
    ) -> Result<(), BuildError> {
        let id = id.into();
        if self.items.contains_key(&id) {
            return Err(make_err(format!("'{}' already registered", id)));
        }
        self.items.insert(id, value);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&V> {
        self.items.get(id)
    }

    pub fn contains_key(&self, id: &str) -> bool {
        self.items.contains_key(id)
    }

    pub fn replace(&mut self, id: impl Into<String>, value: V) -> Option<V> {
        self.items.insert(id.into(), value)
    }

    pub fn remove(&mut self, id: &str) -> Option<V> {
        self.items.remove(id)
    }

    pub fn ids(&self) -> Vec<String> {
        self.items.keys().cloned().collect()
    }
}

impl<V: Clone> MapRegistry<V> {
    pub fn get_cloned(&self, id: &str) -> Option<V> {
        self.items.get(id).cloned()
    }
}

// ---------------------------------------------------------------------------
// Type aliases
// ---------------------------------------------------------------------------

pub type MapToolRegistry = MapRegistry<Arc<dyn Tool>>;
pub type MapProviderRegistry = MapRegistry<Arc<dyn LlmExecutor>>;
pub type MapAgentSpecRegistry = MapRegistry<AgentSpec>;
pub type MapPluginSource = MapRegistry<Arc<dyn Plugin>>;
#[cfg(feature = "a2a")]
pub type MapBackendRegistry = MapRegistry<Arc<dyn ExecutionBackendFactory>>;

// ---------------------------------------------------------------------------
// Convenience register methods (preserve original call-site signatures)
// ---------------------------------------------------------------------------

impl MapToolRegistry {
    pub fn register_tool(
        &mut self,
        id: impl Into<String>,
        tool: Arc<dyn Tool>,
    ) -> Result<(), BuildError> {
        self.register(id, tool, |msg| {
            BuildError::ToolRegistryConflict(format!("tool {msg}"))
        })
    }
}

/// In-memory model registry holding both single models and model pools in one
/// id namespace.
///
/// Derefs to the inner model `MapRegistry<ModelSpec>` so existing model-only
/// call sites (`get`, `ids`, `register_model`, …) keep working unchanged; pool
/// entries live in a parallel map and are reached via the [`ModelRegistry`]
/// pool methods.
pub struct MapModelRegistry {
    models: MapRegistry<ModelSpec>,
    pools: MapRegistry<ModelPoolSpec>,
}

impl Default for MapModelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl Deref for MapModelRegistry {
    type Target = MapRegistry<ModelSpec>;

    fn deref(&self) -> &Self::Target {
        &self.models
    }
}

impl DerefMut for MapModelRegistry {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.models
    }
}

impl MapModelRegistry {
    pub fn new() -> Self {
        Self {
            models: MapRegistry::new(),
            pools: MapRegistry::new(),
        }
    }

    /// Register a `ModelSpec`, extracting the id from `spec.id`.
    ///
    /// This is the single registration chokepoint for every entry point
    /// (`AgentRuntimeBuilder::with_model`, config compilation, lifecycle
    /// rebuilds). It enforces the same semantic rules as the JSON config
    /// surface via [`validate_model_spec_struct`], so a `ModelSpec` cannot
    /// enter a registry with values `validate_model_spec` would reject.
    pub fn register_model(&mut self, spec: ModelSpec) -> Result<(), BuildError> {
        validate_model_spec_struct(&spec)?;
        if self.pools.contains_key(&spec.id) {
            return Err(BuildError::ModelRegistryConflict(format!(
                "model '{}' already registered as a pool",
                spec.id
            )));
        }
        let id = spec.id.clone();
        self.models.register(id, spec, |msg| {
            BuildError::ModelRegistryConflict(format!("model {msg}"))
        })
    }

    /// Register a `ModelPoolSpec`, extracting the id from `spec.id`. Validated
    /// via [`validate_model_pool_spec_struct`]; the id must not collide with a
    /// model or another pool (single shared id namespace).
    pub fn register_model_pool(&mut self, spec: ModelPoolSpec) -> Result<(), BuildError> {
        validate_model_pool_spec_struct(&spec)?;
        if self.models.contains_key(&spec.id) {
            return Err(BuildError::ModelRegistryConflict(format!(
                "pool '{}' already registered as a model",
                spec.id
            )));
        }
        let id = spec.id.clone();
        self.pools.register(id, spec, |msg| {
            BuildError::ModelRegistryConflict(format!("model pool {msg}"))
        })
    }
}

impl MapProviderRegistry {
    pub fn register_provider(
        &mut self,
        id: impl Into<String>,
        executor: Arc<dyn LlmExecutor>,
    ) -> Result<(), BuildError> {
        self.register(id, executor, |msg| {
            BuildError::ProviderRegistryConflict(format!("provider {msg}"))
        })
    }

    pub fn replace_provider(
        &mut self,
        id: impl Into<String>,
        executor: Arc<dyn LlmExecutor>,
    ) -> Option<Arc<dyn LlmExecutor>> {
        self.replace(id, executor)
    }

    pub fn remove_provider(&mut self, id: &str) -> Option<Arc<dyn LlmExecutor>> {
        self.remove(id)
    }
}

impl MapAgentSpecRegistry {
    /// Register an `AgentSpec`, extracting the ID from `spec.id`.
    pub fn register_spec(&mut self, spec: AgentSpec) -> Result<(), BuildError> {
        let id = spec.id.clone();
        self.register(id, spec, |msg| {
            BuildError::AgentRegistryConflict(format!("agent {msg}"))
        })
    }
}

impl MapPluginSource {
    pub fn register_plugin(
        &mut self,
        id: impl Into<String>,
        plugin: Arc<dyn Plugin>,
    ) -> Result<(), BuildError> {
        self.register(id, plugin, |msg| {
            BuildError::PluginRegistryConflict(format!("plugin {msg}"))
        })
    }
}

#[cfg(feature = "a2a")]
impl MapBackendRegistry {
    pub fn register_backend_factory(
        &mut self,
        factory: Arc<dyn ExecutionBackendFactory>,
    ) -> Result<(), BuildError> {
        let backend = factory.backend().to_string();
        self.register(backend, factory, |msg| {
            BuildError::BackendRegistryConflict(format!("backend {msg}"))
        })
    }

    pub fn with_default_remote_backends() -> Self {
        let mut registry = Self::new();
        registry
            .register_backend_factory(Arc::new(A2aBackendFactory))
            .expect("fresh backend registry should accept built-in A2A backend");
        registry
    }
}

// ---------------------------------------------------------------------------
// Trait implementations
// ---------------------------------------------------------------------------

impl ToolRegistry for MapToolRegistry {
    fn get_tool(&self, id: &str) -> Option<Arc<dyn Tool>> {
        self.get_cloned(id)
    }

    fn tool_ids(&self) -> Vec<String> {
        self.ids()
    }
}

impl ModelRegistry for MapModelRegistry {
    fn get_model(&self, id: &str) -> Option<ModelSpec> {
        self.models.get_cloned(id)
    }

    fn model_ids(&self) -> Vec<String> {
        self.models.ids()
    }

    fn get_pool(&self, id: &str) -> Option<ModelPoolSpec> {
        self.pools.get_cloned(id)
    }

    fn pool_ids(&self) -> Vec<String> {
        self.pools.ids()
    }
}

impl ProviderRegistry for MapProviderRegistry {
    fn get_provider(&self, id: &str) -> Option<Arc<dyn LlmExecutor>> {
        self.get_cloned(id)
    }

    fn provider_ids(&self) -> Vec<String> {
        self.ids()
    }
}

impl AgentSpecRegistry for MapAgentSpecRegistry {
    fn get_agent(&self, id: &str) -> Option<AgentSpec> {
        self.get_cloned(id)
    }

    fn agent_ids(&self) -> Vec<String> {
        self.ids()
    }
}

impl PluginSource for MapPluginSource {
    fn get_plugin(&self, id: &str) -> Option<Arc<dyn Plugin>> {
        self.get_cloned(id)
    }

    fn plugin_ids(&self) -> Vec<String> {
        self.ids()
    }
}

#[cfg(feature = "a2a")]
impl BackendRegistry for MapBackendRegistry {
    fn get_backend_factory(&self, backend: &str) -> Option<Arc<dyn ExecutionBackendFactory>> {
        self.get_cloned(backend)
    }

    fn backend_ids(&self) -> Vec<String> {
        self.ids()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::registry_spec::{Modalities, Modality};

    /// Helper to create a simple error constructor for tests.
    fn test_err(msg: String) -> BuildError {
        BuildError::ToolRegistryConflict(msg)
    }

    #[test]
    fn map_model_registry_preserves_full_modelspec() {
        let spec = ModelSpec {
            id: "m".into(),
            provider_id: "p".into(),
            upstream_model: "u".into(),
            context_window: Some(200_000),
            max_output_tokens: Some(8_192),
            modalities: Modalities {
                input: vec![Modality::Text, Modality::Image],
                output: vec![Modality::Text],
            },
            knowledge_cutoff: Some("2026-01".into()),
            input_token_price_per_million_usd: Some(3.0),
            output_token_price_per_million_usd: Some(15.0),
        };
        let mut r = MapModelRegistry::new();
        r.register_model(spec.clone()).expect("first register");
        let got = r.get_model("m").expect("must find");
        assert_eq!(
            got, spec,
            "registry must return the full ModelSpec unchanged"
        );
    }

    #[test]
    fn map_model_registry_rejects_duplicate_id() {
        let mut r = MapModelRegistry::new();
        r.register_model(ModelSpec::new("m", "p", "u1"))
            .expect("first ok");
        let err = r
            .register_model(ModelSpec::new("m", "p", "u2"))
            .unwrap_err();
        assert!(
            matches!(err, BuildError::ModelRegistryConflict(_)),
            "expected ModelRegistryConflict, got: {err:?}"
        );
    }

    #[test]
    fn new_creates_empty_registry() {
        let reg = MapRegistry::<String>::new();
        assert!(reg.ids().is_empty());
    }

    #[test]
    fn register_and_get() {
        let mut reg = MapRegistry::<String>::new();
        reg.register("key1", "value1".into(), test_err).unwrap();
        assert_eq!(reg.get("key1"), Some(&"value1".to_string()));
    }

    #[test]
    fn get_missing_key_returns_none() {
        let reg = MapRegistry::<String>::new();
        assert_eq!(reg.get("missing"), None);
    }

    #[test]
    fn get_cloned_returns_value() {
        let mut reg = MapRegistry::<String>::new();
        reg.register("k", "v".into(), test_err).unwrap();
        assert_eq!(reg.get_cloned("k"), Some("v".to_string()));
    }

    #[test]
    fn get_cloned_missing_key_returns_none() {
        let reg = MapRegistry::<String>::new();
        assert_eq!(reg.get_cloned("nope"), None);
    }

    #[test]
    fn ids_empty_registry() {
        let reg = MapRegistry::<i32>::new();
        assert!(reg.ids().is_empty());
    }

    #[test]
    fn ids_returns_all_keys() {
        let mut reg = MapRegistry::<i32>::new();
        reg.register("a", 1, test_err).unwrap();
        reg.register("b", 2, test_err).unwrap();
        reg.register("c", 3, test_err).unwrap();

        let mut ids = reg.ids();
        ids.sort();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn register_duplicate_returns_error() {
        let mut reg = MapRegistry::<String>::new();
        reg.register("dup", "first".into(), test_err).unwrap();
        let err = reg.register("dup", "second".into(), test_err).unwrap_err();
        assert!(err.to_string().contains("already registered"));
    }
}
