//! In-memory HashMap-backed registry implementations.

use std::collections::HashMap;
use std::sync::Arc;

use crate::builder::BuildError;
use crate::plugins::Plugin;
use awaken_contract::contract::executor::LlmExecutor;
use awaken_contract::contract::tool::Tool;

use super::traits::{
    AgentSpecRegistry, ModelEntry, ModelRegistry, PluginSource, ProviderRegistry, ToolRegistry,
};
use awaken_contract::registry_spec::AgentSpec;

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
pub type MapModelRegistry = MapRegistry<ModelEntry>;
pub type MapProviderRegistry = MapRegistry<Arc<dyn LlmExecutor>>;
pub type MapAgentSpecRegistry = MapRegistry<AgentSpec>;
pub type MapPluginSource = MapRegistry<Arc<dyn Plugin>>;

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

impl MapModelRegistry {
    pub fn register_model(
        &mut self,
        id: impl Into<String>,
        entry: ModelEntry,
    ) -> Result<(), BuildError> {
        self.register(id, entry, |msg| {
            BuildError::ModelRegistryConflict(format!("model {msg}"))
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
    fn get_model(&self, id: &str) -> Option<&ModelEntry> {
        self.get(id)
    }
}

impl ProviderRegistry for MapProviderRegistry {
    fn get_provider(&self, id: &str) -> Option<Arc<dyn LlmExecutor>> {
        self.get_cloned(id)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a simple error constructor for tests.
    fn test_err(msg: String) -> BuildError {
        BuildError::ToolRegistryConflict(msg)
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
