use super::sorted_registry_ids;
use super::traits::{BehaviorRegistry, BehaviorRegistryError};
use crate::contracts::runtime::AgentBehavior;
use std::collections::HashMap;
use std::sync::Arc;

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
