//! In-memory HashMap-backed registry implementations.

use std::collections::HashMap;
use std::sync::Arc;

use crate::contract::executor::LlmExecutor;
use crate::contract::tool::Tool;
use crate::plugins::Plugin;

use super::spec::AgentSpec;
use super::traits::{
    AgentSpecRegistry, ModelEntry, ModelRegistry, PluginSource, ProviderRegistry, ToolRegistry,
};

// ---------------------------------------------------------------------------
// MapToolRegistry
// ---------------------------------------------------------------------------

/// In-memory tool registry backed by a `HashMap`.
#[derive(Default)]
pub struct MapToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl MapToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, id: impl Into<String>, tool: Arc<dyn Tool>) {
        self.tools.insert(id.into(), tool);
    }
}

impl ToolRegistry for MapToolRegistry {
    fn get_tool(&self, id: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(id).cloned()
    }

    fn tool_ids(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }
}

// ---------------------------------------------------------------------------
// MapModelRegistry
// ---------------------------------------------------------------------------

/// In-memory model registry backed by a `HashMap`.
#[derive(Default)]
pub struct MapModelRegistry {
    models: HashMap<String, ModelEntry>,
}

impl MapModelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, id: impl Into<String>, entry: ModelEntry) {
        self.models.insert(id.into(), entry);
    }
}

impl ModelRegistry for MapModelRegistry {
    fn get_model(&self, id: &str) -> Option<&ModelEntry> {
        self.models.get(id)
    }
}

// ---------------------------------------------------------------------------
// MapProviderRegistry
// ---------------------------------------------------------------------------

/// In-memory provider registry backed by a `HashMap`.
#[derive(Default)]
pub struct MapProviderRegistry {
    providers: HashMap<String, Arc<dyn LlmExecutor>>,
}

impl MapProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, id: impl Into<String>, executor: Arc<dyn LlmExecutor>) {
        self.providers.insert(id.into(), executor);
    }
}

impl ProviderRegistry for MapProviderRegistry {
    fn get_provider(&self, id: &str) -> Option<Arc<dyn LlmExecutor>> {
        self.providers.get(id).cloned()
    }
}

// ---------------------------------------------------------------------------
// MapAgentSpecRegistry
// ---------------------------------------------------------------------------

/// In-memory agent spec registry backed by a `HashMap`.
#[derive(Default)]
pub struct MapAgentSpecRegistry {
    agents: HashMap<String, AgentSpec>,
}

impl MapAgentSpecRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, spec: AgentSpec) {
        self.agents.insert(spec.id.clone(), spec);
    }
}

impl AgentSpecRegistry for MapAgentSpecRegistry {
    fn get_agent(&self, id: &str) -> Option<&AgentSpec> {
        self.agents.get(id)
    }

    fn agent_ids(&self) -> Vec<String> {
        self.agents.keys().cloned().collect()
    }
}

// ---------------------------------------------------------------------------
// MapPluginSource
// ---------------------------------------------------------------------------

/// In-memory plugin source backed by a `HashMap`.
#[derive(Default)]
pub struct MapPluginSource {
    plugins: HashMap<String, Arc<dyn Plugin>>,
}

impl MapPluginSource {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, id: impl Into<String>, plugin: Arc<dyn Plugin>) {
        self.plugins.insert(id.into(), plugin);
    }
}

impl PluginSource for MapPluginSource {
    fn get_plugin(&self, id: &str) -> Option<Arc<dyn Plugin>> {
        self.plugins.get(id).cloned()
    }
}
