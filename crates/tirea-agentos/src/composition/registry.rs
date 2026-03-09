use super::{
    AgentRegistry, AgentRegistryError, BehaviorRegistry, BehaviorRegistryError, ModelDefinition,
    ModelRegistry, ModelRegistryError, ProviderRegistry, ProviderRegistryError, ToolRegistry,
    ToolRegistryError,
};
use crate::contracts::runtime::tool_call::Tool;
use crate::contracts::runtime::AgentBehavior;
use crate::runtime::AgentDefinition;
use genai::Client;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

fn sorted_registry_ids<T>(entries: &HashMap<String, T>) -> Vec<String> {
    let mut ids: Vec<String> = entries.keys().cloned().collect();
    ids.sort();
    ids
}

#[derive(Clone, Default)]
pub struct InMemoryProviderRegistry {
    providers: HashMap<String, Client>,
}

impl std::fmt::Debug for InMemoryProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryProviderRegistry")
            .field("len", &self.providers.len())
            .finish()
    }
}

impl InMemoryProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &mut self,
        provider_id: impl Into<String>,
        client: Client,
    ) -> Result<(), ProviderRegistryError> {
        let provider_id = provider_id.into();
        if provider_id.trim().is_empty() {
            return Err(ProviderRegistryError::EmptyProviderId);
        }
        if self.providers.contains_key(&provider_id) {
            return Err(ProviderRegistryError::ProviderIdConflict(provider_id));
        }
        self.providers.insert(provider_id, client);
        Ok(())
    }

    pub fn extend(
        &mut self,
        providers: HashMap<String, Client>,
    ) -> Result<(), ProviderRegistryError> {
        for (id, client) in providers {
            self.register(id, client)?;
        }
        Ok(())
    }
}

impl ProviderRegistry for InMemoryProviderRegistry {
    fn len(&self) -> usize {
        self.providers.len()
    }

    fn get(&self, id: &str) -> Option<Client> {
        self.providers.get(id).cloned()
    }

    fn ids(&self) -> Vec<String> {
        sorted_registry_ids(&self.providers)
    }

    fn snapshot(&self) -> HashMap<String, Client> {
        self.providers.clone()
    }
}

#[derive(Clone, Default)]
pub struct CompositeProviderRegistry {
    merged: InMemoryProviderRegistry,
}

impl std::fmt::Debug for CompositeProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompositeProviderRegistry")
            .field("len", &self.merged.len())
            .finish()
    }
}

impl CompositeProviderRegistry {
    pub fn try_new(
        regs: impl IntoIterator<Item = Arc<dyn ProviderRegistry>>,
    ) -> Result<Self, ProviderRegistryError> {
        let mut merged = InMemoryProviderRegistry::new();
        for r in regs {
            merged.extend(r.snapshot())?;
        }
        Ok(Self { merged })
    }
}

impl ProviderRegistry for CompositeProviderRegistry {
    fn len(&self) -> usize {
        self.merged.len()
    }

    fn get(&self, id: &str) -> Option<Client> {
        self.merged.get(id)
    }

    fn ids(&self) -> Vec<String> {
        sorted_registry_ids(&self.merged.providers)
    }

    fn snapshot(&self) -> HashMap<String, Client> {
        self.merged.snapshot()
    }
}

#[derive(Clone, Default)]
pub struct InMemoryBehaviorRegistry {
    behaviors: HashMap<String, Arc<dyn AgentBehavior>>,
}

impl std::fmt::Debug for InMemoryBehaviorRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryBehaviorRegistry")
            .field("len", &self.behaviors.len())
            .finish()
    }
}

impl InMemoryBehaviorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &mut self,
        behavior: Arc<dyn AgentBehavior>,
    ) -> Result<(), BehaviorRegistryError> {
        let id = behavior.id().to_string();
        if self.behaviors.contains_key(&id) {
            return Err(BehaviorRegistryError::BehaviorIdConflict(id));
        }
        self.behaviors.insert(id, behavior);
        Ok(())
    }

    pub fn register_named(
        &mut self,
        id: impl Into<String>,
        behavior: Arc<dyn AgentBehavior>,
    ) -> Result<(), BehaviorRegistryError> {
        let key = id.into();
        let behavior_id = behavior.id().to_string();
        if key != behavior_id {
            return Err(BehaviorRegistryError::BehaviorIdMismatch { key, behavior_id });
        }
        if self.behaviors.contains_key(&key) {
            return Err(BehaviorRegistryError::BehaviorIdConflict(key));
        }
        self.behaviors.insert(key, behavior);
        Ok(())
    }

    pub fn extend_named(
        &mut self,
        behaviors: HashMap<String, Arc<dyn AgentBehavior>>,
    ) -> Result<(), BehaviorRegistryError> {
        for (key, behavior) in behaviors {
            self.register_named(key, behavior)?;
        }
        Ok(())
    }

    pub fn extend_registry(
        &mut self,
        other: &dyn BehaviorRegistry,
    ) -> Result<(), BehaviorRegistryError> {
        self.extend_named(other.snapshot())
    }
}

impl BehaviorRegistry for InMemoryBehaviorRegistry {
    fn len(&self) -> usize {
        self.behaviors.len()
    }

    fn get(&self, id: &str) -> Option<Arc<dyn AgentBehavior>> {
        self.behaviors.get(id).cloned()
    }

    fn ids(&self) -> Vec<String> {
        sorted_registry_ids(&self.behaviors)
    }

    fn snapshot(&self) -> HashMap<String, Arc<dyn AgentBehavior>> {
        self.behaviors.clone()
    }
}

#[derive(Clone, Default)]
pub struct CompositeBehaviorRegistry {
    merged: InMemoryBehaviorRegistry,
}

impl std::fmt::Debug for CompositeBehaviorRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompositeBehaviorRegistry")
            .field("len", &self.merged.len())
            .finish()
    }
}

impl CompositeBehaviorRegistry {
    pub fn try_new(
        regs: impl IntoIterator<Item = Arc<dyn BehaviorRegistry>>,
    ) -> Result<Self, BehaviorRegistryError> {
        let mut merged = InMemoryBehaviorRegistry::new();
        for r in regs {
            merged.extend_registry(r.as_ref())?;
        }
        Ok(Self { merged })
    }
}

impl BehaviorRegistry for CompositeBehaviorRegistry {
    fn len(&self) -> usize {
        self.merged.len()
    }

    fn get(&self, id: &str) -> Option<Arc<dyn AgentBehavior>> {
        self.merged.get(id)
    }

    fn ids(&self) -> Vec<String> {
        sorted_registry_ids(&self.merged.behaviors)
    }

    fn snapshot(&self) -> HashMap<String, Arc<dyn AgentBehavior>> {
        self.merged.snapshot()
    }
}

#[derive(Clone, Default)]
pub struct InMemoryToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl std::fmt::Debug for InMemoryToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryToolRegistry")
            .field("len", &self.tools.len())
            .finish()
    }
}

impl InMemoryToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(id).cloned()
    }

    pub fn ids(&self) -> impl Iterator<Item = &String> {
        self.tools.keys()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), ToolRegistryError> {
        let id = tool.descriptor().id;
        if self.tools.contains_key(&id) {
            return Err(ToolRegistryError::ToolIdConflict(id));
        }
        self.tools.insert(id, tool);
        Ok(())
    }

    pub fn register_named(
        &mut self,
        id: impl Into<String>,
        tool: Arc<dyn Tool>,
    ) -> Result<(), ToolRegistryError> {
        let key = id.into();
        let descriptor_id = tool.descriptor().id;
        if key != descriptor_id {
            return Err(ToolRegistryError::ToolIdMismatch { key, descriptor_id });
        }
        if self.tools.contains_key(&key) {
            return Err(ToolRegistryError::ToolIdConflict(key));
        }
        self.tools.insert(key, tool);
        Ok(())
    }

    pub fn extend_named(
        &mut self,
        tools: HashMap<String, Arc<dyn Tool>>,
    ) -> Result<(), ToolRegistryError> {
        for (key, tool) in tools {
            self.register_named(key, tool)?;
        }
        Ok(())
    }

    pub fn extend_registry(&mut self, other: &dyn ToolRegistry) -> Result<(), ToolRegistryError> {
        self.extend_named(other.snapshot())
    }

    pub fn merge_many(
        regs: impl IntoIterator<Item = InMemoryToolRegistry>,
    ) -> Result<InMemoryToolRegistry, ToolRegistryError> {
        let mut out = InMemoryToolRegistry::new();
        for r in regs {
            out.extend_named(r.into_map())?;
        }
        Ok(out)
    }

    pub fn into_map(self) -> HashMap<String, Arc<dyn Tool>> {
        self.tools
    }

    pub fn to_map(&self) -> HashMap<String, Arc<dyn Tool>> {
        self.tools.clone()
    }
}

impl ToolRegistry for InMemoryToolRegistry {
    fn len(&self) -> usize {
        self.len()
    }

    fn get(&self, id: &str) -> Option<Arc<dyn Tool>> {
        self.get(id)
    }

    fn ids(&self) -> Vec<String> {
        sorted_registry_ids(&self.tools)
    }

    fn snapshot(&self) -> HashMap<String, Arc<dyn Tool>> {
        self.tools.clone()
    }
}

#[derive(Clone, Default)]
pub struct CompositeToolRegistry {
    registries: Vec<Arc<dyn ToolRegistry>>,
    cached_snapshot: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>>,
}

impl std::fmt::Debug for CompositeToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snapshot = match self.cached_snapshot.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        f.debug_struct("CompositeToolRegistry")
            .field("registries", &self.registries.len())
            .field("len", &snapshot.len())
            .finish()
    }
}

impl CompositeToolRegistry {
    pub fn try_new(
        regs: impl IntoIterator<Item = Arc<dyn ToolRegistry>>,
    ) -> Result<Self, ToolRegistryError> {
        let registries: Vec<Arc<dyn ToolRegistry>> = regs.into_iter().collect();
        let merged = Self::merge_snapshots(&registries)?;
        Ok(Self {
            registries,
            cached_snapshot: Arc::new(RwLock::new(merged)),
        })
    }

    fn merge_snapshots(
        registries: &[Arc<dyn ToolRegistry>],
    ) -> Result<HashMap<String, Arc<dyn Tool>>, ToolRegistryError> {
        let mut merged = InMemoryToolRegistry::new();
        for reg in registries {
            merged.extend_registry(reg.as_ref())?;
        }
        Ok(merged.into_map())
    }

    fn refresh_snapshot(&self) -> Result<HashMap<String, Arc<dyn Tool>>, ToolRegistryError> {
        Self::merge_snapshots(&self.registries)
    }

    fn read_cached_snapshot(&self) -> HashMap<String, Arc<dyn Tool>> {
        match self.cached_snapshot.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn write_cached_snapshot(&self, snapshot: HashMap<String, Arc<dyn Tool>>) {
        match self.cached_snapshot.write() {
            Ok(mut guard) => *guard = snapshot,
            Err(poisoned) => *poisoned.into_inner() = snapshot,
        };
    }
}

impl ToolRegistry for CompositeToolRegistry {
    fn len(&self) -> usize {
        self.snapshot().len()
    }

    fn get(&self, id: &str) -> Option<Arc<dyn Tool>> {
        self.snapshot().get(id).cloned()
    }

    fn ids(&self) -> Vec<String> {
        let snapshot = self.snapshot();
        sorted_registry_ids(&snapshot)
    }

    fn snapshot(&self) -> HashMap<String, Arc<dyn Tool>> {
        match self.refresh_snapshot() {
            Ok(snapshot) => {
                self.write_cached_snapshot(snapshot.clone());
                snapshot
            }
            Err(_) => self.read_cached_snapshot(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct InMemoryAgentRegistry {
    agents: HashMap<String, AgentDefinition>,
}

impl InMemoryAgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.agents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    pub fn get(&self, id: &str) -> Option<AgentDefinition> {
        self.agents.get(id).cloned()
    }

    pub fn ids(&self) -> impl Iterator<Item = &String> {
        self.agents.keys()
    }

    pub fn register(
        &mut self,
        agent_id: impl Into<String>,
        mut def: AgentDefinition,
    ) -> Result<(), AgentRegistryError> {
        let agent_id = agent_id.into();
        if self.agents.contains_key(&agent_id) {
            return Err(AgentRegistryError::AgentIdConflict(agent_id));
        }
        // The registry key is canonical to avoid mismatches.
        def.id = agent_id.clone();
        self.agents.insert(agent_id, def);
        Ok(())
    }

    pub fn upsert(&mut self, agent_id: impl Into<String>, mut def: AgentDefinition) {
        let agent_id = agent_id.into();
        def.id = agent_id.clone();
        self.agents.insert(agent_id, def);
    }

    pub fn extend_upsert(&mut self, defs: HashMap<String, AgentDefinition>) {
        for (id, def) in defs {
            self.upsert(id, def);
        }
    }

    pub fn extend_registry(&mut self, other: &dyn AgentRegistry) -> Result<(), AgentRegistryError> {
        for (id, def) in other.snapshot() {
            self.register(id, def)?;
        }
        Ok(())
    }
}

impl AgentRegistry for InMemoryAgentRegistry {
    fn len(&self) -> usize {
        self.len()
    }

    fn get(&self, id: &str) -> Option<AgentDefinition> {
        self.get(id)
    }

    fn ids(&self) -> Vec<String> {
        sorted_registry_ids(&self.agents)
    }

    fn snapshot(&self) -> HashMap<String, AgentDefinition> {
        self.agents.clone()
    }
}

#[derive(Clone, Default)]
pub struct CompositeAgentRegistry {
    registries: Vec<Arc<dyn AgentRegistry>>,
    cached_snapshot: Arc<RwLock<HashMap<String, AgentDefinition>>>,
}

impl std::fmt::Debug for CompositeAgentRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snapshot = match self.cached_snapshot.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        f.debug_struct("CompositeAgentRegistry")
            .field("registries", &self.registries.len())
            .field("len", &snapshot.len())
            .finish()
    }
}

impl CompositeAgentRegistry {
    pub fn try_new(
        regs: impl IntoIterator<Item = Arc<dyn AgentRegistry>>,
    ) -> Result<Self, AgentRegistryError> {
        let registries: Vec<Arc<dyn AgentRegistry>> = regs.into_iter().collect();
        let merged = Self::merge_snapshots(&registries)?;
        Ok(Self {
            registries,
            cached_snapshot: Arc::new(RwLock::new(merged)),
        })
    }

    fn merge_snapshots(
        registries: &[Arc<dyn AgentRegistry>],
    ) -> Result<HashMap<String, AgentDefinition>, AgentRegistryError> {
        let mut merged = InMemoryAgentRegistry::new();
        for reg in registries {
            merged.extend_registry(reg.as_ref())?;
        }
        Ok(merged.snapshot())
    }

    fn refresh_snapshot(&self) -> Result<HashMap<String, AgentDefinition>, AgentRegistryError> {
        Self::merge_snapshots(&self.registries)
    }

    fn read_cached_snapshot(&self) -> HashMap<String, AgentDefinition> {
        match self.cached_snapshot.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn write_cached_snapshot(&self, snapshot: HashMap<String, AgentDefinition>) {
        match self.cached_snapshot.write() {
            Ok(mut guard) => *guard = snapshot,
            Err(poisoned) => *poisoned.into_inner() = snapshot,
        };
    }
}

impl AgentRegistry for CompositeAgentRegistry {
    fn len(&self) -> usize {
        self.snapshot().len()
    }

    fn get(&self, id: &str) -> Option<AgentDefinition> {
        self.snapshot().get(id).cloned()
    }

    fn ids(&self) -> Vec<String> {
        let snapshot = self.snapshot();
        sorted_registry_ids(&snapshot)
    }

    fn snapshot(&self) -> HashMap<String, AgentDefinition> {
        match self.refresh_snapshot() {
            Ok(snapshot) => {
                self.write_cached_snapshot(snapshot.clone());
                snapshot
            }
            Err(_) => self.read_cached_snapshot(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct InMemoryModelRegistry {
    models: HashMap<String, ModelDefinition>,
}

impl InMemoryModelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.models.len()
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    pub fn get(&self, id: &str) -> Option<ModelDefinition> {
        self.models.get(id).cloned()
    }

    pub fn ids(&self) -> impl Iterator<Item = &String> {
        self.models.keys()
    }

    pub fn register(
        &mut self,
        model_id: impl Into<String>,
        mut def: ModelDefinition,
    ) -> Result<(), ModelRegistryError> {
        let model_id = model_id.into();
        if self.models.contains_key(&model_id) {
            return Err(ModelRegistryError::ModelIdConflict(model_id));
        }
        def.provider = def.provider.trim().to_string();
        def.model = def.model.trim().to_string();
        if def.provider.is_empty() {
            return Err(ModelRegistryError::EmptyProviderId);
        }
        if def.model.is_empty() {
            return Err(ModelRegistryError::EmptyModelName);
        }
        self.models.insert(model_id, def);
        Ok(())
    }

    pub fn extend(
        &mut self,
        defs: HashMap<String, ModelDefinition>,
    ) -> Result<(), ModelRegistryError> {
        for (id, def) in defs {
            self.register(id, def)?;
        }
        Ok(())
    }

    pub fn extend_registry(&mut self, other: &dyn ModelRegistry) -> Result<(), ModelRegistryError> {
        self.extend(other.snapshot())
    }
}

impl ModelRegistry for InMemoryModelRegistry {
    fn len(&self) -> usize {
        self.len()
    }

    fn get(&self, id: &str) -> Option<ModelDefinition> {
        self.get(id)
    }

    fn ids(&self) -> Vec<String> {
        sorted_registry_ids(&self.models)
    }

    fn snapshot(&self) -> HashMap<String, ModelDefinition> {
        self.models.clone()
    }
}

#[derive(Clone, Default)]
pub struct CompositeModelRegistry {
    merged: InMemoryModelRegistry,
}

impl std::fmt::Debug for CompositeModelRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompositeModelRegistry")
            .field("len", &self.merged.len())
            .finish()
    }
}

impl CompositeModelRegistry {
    pub fn try_new(
        regs: impl IntoIterator<Item = Arc<dyn ModelRegistry>>,
    ) -> Result<Self, ModelRegistryError> {
        let mut merged = InMemoryModelRegistry::new();
        for r in regs {
            merged.extend_registry(r.as_ref())?;
        }
        Ok(Self { merged })
    }
}

impl ModelRegistry for CompositeModelRegistry {
    fn len(&self) -> usize {
        self.merged.len()
    }

    fn get(&self, id: &str) -> Option<ModelDefinition> {
        self.merged.get(id)
    }

    fn ids(&self) -> Vec<String> {
        sorted_registry_ids(&self.merged.models)
    }

    fn snapshot(&self) -> HashMap<String, ModelDefinition> {
        self.merged.models.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::runtime::tool_call::{ToolDescriptor, ToolError, ToolResult};
    use crate::contracts::ToolCallContext;
    use serde_json::json;

    struct StaticTool {
        descriptor: ToolDescriptor,
    }

    impl StaticTool {
        fn new(id: &str) -> Self {
            Self {
                descriptor: ToolDescriptor::new(id, id, "test tool"),
            }
        }
    }

    #[async_trait::async_trait]
    impl Tool for StaticTool {
        fn descriptor(&self) -> ToolDescriptor {
            self.descriptor.clone()
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success(
                self.descriptor.id.clone(),
                json!({"ok": true}),
            ))
        }
    }

    #[derive(Default)]
    struct MutableToolRegistry {
        tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
    }

    impl MutableToolRegistry {
        fn replace_ids(&self, ids: &[&str]) {
            let mut map = HashMap::new();
            for id in ids {
                map.insert(
                    (*id).to_string(),
                    Arc::new(StaticTool::new(id)) as Arc<dyn Tool>,
                );
            }
            match self.tools.write() {
                Ok(mut guard) => *guard = map,
                Err(poisoned) => *poisoned.into_inner() = map,
            }
        }
    }

    impl ToolRegistry for MutableToolRegistry {
        fn len(&self) -> usize {
            self.snapshot().len()
        }

        fn get(&self, id: &str) -> Option<Arc<dyn Tool>> {
            self.snapshot().get(id).cloned()
        }

        fn ids(&self) -> Vec<String> {
            let mut ids: Vec<String> = self.snapshot().keys().cloned().collect();
            ids.sort();
            ids
        }

        fn snapshot(&self) -> HashMap<String, Arc<dyn Tool>> {
            match self.tools.read() {
                Ok(guard) => guard.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            }
        }
    }

    #[test]
    fn composite_tool_registry_reads_live_updates_from_source_registries() {
        let dynamic = Arc::new(MutableToolRegistry::default());
        dynamic.replace_ids(&["dynamic_a"]);

        let mut static_registry = InMemoryToolRegistry::new();
        static_registry
            .register_named("static_tool", Arc::new(StaticTool::new("static_tool")))
            .expect("register static tool");

        let composite = CompositeToolRegistry::try_new(vec![
            dynamic.clone() as Arc<dyn ToolRegistry>,
            Arc::new(static_registry) as Arc<dyn ToolRegistry>,
        ])
        .expect("compose registries");

        assert!(composite.ids().contains(&"dynamic_a".to_string()));
        assert!(composite.ids().contains(&"static_tool".to_string()));

        dynamic.replace_ids(&["dynamic_a", "dynamic_b"]);

        let ids = composite.ids();
        assert!(ids.contains(&"dynamic_a".to_string()));
        assert!(ids.contains(&"dynamic_b".to_string()));
        assert!(ids.contains(&"static_tool".to_string()));
    }

    #[test]
    fn model_registry_trims_provider_and_model_names() {
        let mut registry = InMemoryModelRegistry::new();
        registry
            .register("m1", ModelDefinition::new(" openai ", " gemini-2.5-flash "))
            .expect("register model");

        let model = registry.get("m1").expect("stored model");
        assert_eq!(model.provider, "openai");
        assert_eq!(model.model, "gemini-2.5-flash");
    }

    #[test]
    fn model_registry_rejects_whitespace_only_provider_or_model() {
        let mut registry = InMemoryModelRegistry::new();
        assert!(matches!(
            registry.register("m1", ModelDefinition::new("   ", "gpt-4o-mini")),
            Err(ModelRegistryError::EmptyProviderId)
        ));
        assert!(matches!(
            registry.register("m2", ModelDefinition::new("openai", "   ")),
            Err(ModelRegistryError::EmptyModelName)
        ));
    }

    #[test]
    fn composite_tool_registry_keeps_last_good_snapshot_on_runtime_conflict() {
        let reg_a = Arc::new(MutableToolRegistry::default());
        reg_a.replace_ids(&["tool_a"]);

        let reg_b = Arc::new(MutableToolRegistry::default());
        reg_b.replace_ids(&["tool_b"]);

        let composite = CompositeToolRegistry::try_new(vec![
            reg_a.clone() as Arc<dyn ToolRegistry>,
            reg_b.clone() as Arc<dyn ToolRegistry>,
        ])
        .expect("compose registries");

        let initial_ids = composite.ids();
        assert_eq!(
            initial_ids,
            vec!["tool_a".to_string(), "tool_b".to_string()]
        );

        // Introduce a conflict at runtime. Composite should fall back to last good snapshot.
        reg_b.replace_ids(&["tool_a"]);

        assert_eq!(composite.ids(), initial_ids);
        assert!(composite.get("tool_b").is_some());
    }

    #[derive(Default)]
    struct MutableAgentRegistry {
        agents: RwLock<HashMap<String, AgentDefinition>>,
    }

    impl MutableAgentRegistry {
        fn replace_ids(&self, ids: &[&str]) {
            let mut map = HashMap::new();
            for id in ids {
                map.insert((*id).to_string(), AgentDefinition::new("gpt-4o-mini"));
            }
            match self.agents.write() {
                Ok(mut guard) => *guard = map,
                Err(poisoned) => *poisoned.into_inner() = map,
            }
        }
    }

    impl AgentRegistry for MutableAgentRegistry {
        fn len(&self) -> usize {
            self.snapshot().len()
        }

        fn get(&self, id: &str) -> Option<AgentDefinition> {
            self.snapshot().get(id).cloned()
        }

        fn ids(&self) -> Vec<String> {
            let mut ids: Vec<String> = self.snapshot().keys().cloned().collect();
            ids.sort();
            ids
        }

        fn snapshot(&self) -> HashMap<String, AgentDefinition> {
            match self.agents.read() {
                Ok(guard) => guard.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            }
        }
    }

    #[test]
    fn composite_agent_registry_reads_live_updates_from_source_registries() {
        let dynamic = Arc::new(MutableAgentRegistry::default());
        dynamic.replace_ids(&["agent_a"]);

        let mut static_registry = InMemoryAgentRegistry::new();
        static_registry.upsert("agent_static", AgentDefinition::new("gpt-4o-mini"));

        let composite = CompositeAgentRegistry::try_new(vec![
            dynamic.clone() as Arc<dyn AgentRegistry>,
            Arc::new(static_registry) as Arc<dyn AgentRegistry>,
        ])
        .expect("compose registries");

        assert!(composite.ids().contains(&"agent_a".to_string()));
        assert!(composite.ids().contains(&"agent_static".to_string()));

        dynamic.replace_ids(&["agent_a", "agent_b"]);
        let ids = composite.ids();
        assert!(ids.contains(&"agent_a".to_string()));
        assert!(ids.contains(&"agent_b".to_string()));
        assert!(ids.contains(&"agent_static".to_string()));
    }

    #[test]
    fn composite_agent_registry_keeps_last_good_snapshot_on_runtime_conflict() {
        let reg_a = Arc::new(MutableAgentRegistry::default());
        reg_a.replace_ids(&["agent_a"]);

        let reg_b = Arc::new(MutableAgentRegistry::default());
        reg_b.replace_ids(&["agent_b"]);

        let composite = CompositeAgentRegistry::try_new(vec![
            reg_a.clone() as Arc<dyn AgentRegistry>,
            reg_b.clone() as Arc<dyn AgentRegistry>,
        ])
        .expect("compose registries");

        let initial_ids = composite.ids();
        assert_eq!(
            initial_ids,
            vec!["agent_a".to_string(), "agent_b".to_string()]
        );

        reg_b.replace_ids(&["agent_a"]);
        assert_eq!(composite.ids(), initial_ids);
        assert!(composite.get("agent_b").is_some());
    }
}
