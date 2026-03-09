use super::sorted_registry_ids;
use super::traits::{AgentRegistry, AgentRegistryError};
use crate::composition::AgentDefinition;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

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

#[cfg(test)]
mod tests {
    use super::*;

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
