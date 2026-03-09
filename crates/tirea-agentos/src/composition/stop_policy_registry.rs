use super::super::StopPolicy;
use std::collections::HashMap;
use std::sync::Arc;

fn sorted_registry_ids<T>(entries: &HashMap<String, T>) -> Vec<String> {
    let mut ids: Vec<String> = entries.keys().cloned().collect();
    ids.sort();
    ids
}

#[derive(Debug, thiserror::Error)]
pub enum StopPolicyRegistryError {
    #[error("stop policy id already registered: {0}")]
    StopPolicyIdConflict(String),

    #[error("stop policy id mismatch: key={key} policy.name()={policy_name}")]
    StopPolicyIdMismatch { key: String, policy_name: String },
}

pub trait StopPolicyRegistry: Send + Sync {
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn get(&self, id: &str) -> Option<Arc<dyn StopPolicy>>;

    fn ids(&self) -> Vec<String>;

    fn snapshot(&self) -> HashMap<String, Arc<dyn StopPolicy>>;
}

#[derive(Clone, Default)]
pub struct InMemoryStopPolicyRegistry {
    policies: HashMap<String, Arc<dyn StopPolicy>>,
}

impl std::fmt::Debug for InMemoryStopPolicyRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryStopPolicyRegistry")
            .field("len", &self.policies.len())
            .finish()
    }
}

impl InMemoryStopPolicyRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_named(
        &mut self,
        id: impl Into<String>,
        policy: Arc<dyn StopPolicy>,
    ) -> Result<(), StopPolicyRegistryError> {
        let key = id.into();
        if self.policies.contains_key(&key) {
            return Err(StopPolicyRegistryError::StopPolicyIdConflict(key));
        }
        self.policies.insert(key, policy);
        Ok(())
    }

    pub fn extend_named(
        &mut self,
        policies: HashMap<String, Arc<dyn StopPolicy>>,
    ) -> Result<(), StopPolicyRegistryError> {
        for (key, policy) in policies {
            self.register_named(key, policy)?;
        }
        Ok(())
    }

    pub fn extend_registry(
        &mut self,
        other: &dyn StopPolicyRegistry,
    ) -> Result<(), StopPolicyRegistryError> {
        self.extend_named(other.snapshot())
    }
}

impl StopPolicyRegistry for InMemoryStopPolicyRegistry {
    fn len(&self) -> usize {
        self.policies.len()
    }

    fn get(&self, id: &str) -> Option<Arc<dyn StopPolicy>> {
        self.policies.get(id).cloned()
    }

    fn ids(&self) -> Vec<String> {
        sorted_registry_ids(&self.policies)
    }

    fn snapshot(&self) -> HashMap<String, Arc<dyn StopPolicy>> {
        self.policies.clone()
    }
}

#[derive(Clone, Default)]
pub struct CompositeStopPolicyRegistry {
    merged: InMemoryStopPolicyRegistry,
}

impl std::fmt::Debug for CompositeStopPolicyRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompositeStopPolicyRegistry")
            .field("len", &self.merged.len())
            .finish()
    }
}

impl CompositeStopPolicyRegistry {
    pub fn try_new(
        regs: impl IntoIterator<Item = Arc<dyn StopPolicyRegistry>>,
    ) -> Result<Self, StopPolicyRegistryError> {
        let mut merged = InMemoryStopPolicyRegistry::new();
        for r in regs {
            merged.extend_registry(r.as_ref())?;
        }
        Ok(Self { merged })
    }
}

impl StopPolicyRegistry for CompositeStopPolicyRegistry {
    fn len(&self) -> usize {
        self.merged.len()
    }

    fn get(&self, id: &str) -> Option<Arc<dyn StopPolicy>> {
        self.merged.get(id)
    }

    fn ids(&self) -> Vec<String> {
        self.merged.ids()
    }

    fn snapshot(&self) -> HashMap<String, Arc<dyn StopPolicy>> {
        self.merged.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::StoppedReason;
    use crate::runtime::StopPolicyInput;

    #[derive(Debug)]
    struct MockStopPolicy {
        name: String,
    }

    impl MockStopPolicy {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_string(),
            }
        }
    }

    impl StopPolicy for MockStopPolicy {
        fn id(&self) -> &str {
            &self.name
        }

        fn evaluate(&self, _input: &StopPolicyInput<'_>) -> Option<StoppedReason> {
            None
        }
    }

    #[test]
    fn in_memory_register_and_get() {
        let mut reg = InMemoryStopPolicyRegistry::new();
        reg.register_named("max_rounds", Arc::new(MockStopPolicy::new("max_rounds")))
            .unwrap();
        assert_eq!(reg.len(), 1);
        assert!(reg.get("max_rounds").is_some());
        assert!(reg.get("other").is_none());
    }

    #[test]
    fn in_memory_rejects_duplicate() {
        let mut reg = InMemoryStopPolicyRegistry::new();
        reg.register_named("p1", Arc::new(MockStopPolicy::new("p1")))
            .unwrap();
        let err = reg
            .register_named("p1", Arc::new(MockStopPolicy::new("p1")))
            .unwrap_err();
        assert!(matches!(
            err,
            StopPolicyRegistryError::StopPolicyIdConflict(ref id) if id == "p1"
        ));
    }

    #[test]
    fn composite_merges_registries() {
        let mut r1 = InMemoryStopPolicyRegistry::new();
        r1.register_named("p1", Arc::new(MockStopPolicy::new("p1")))
            .unwrap();
        let mut r2 = InMemoryStopPolicyRegistry::new();
        r2.register_named("p2", Arc::new(MockStopPolicy::new("p2")))
            .unwrap();

        let composite = CompositeStopPolicyRegistry::try_new(vec![
            Arc::new(r1) as Arc<dyn StopPolicyRegistry>,
            Arc::new(r2) as Arc<dyn StopPolicyRegistry>,
        ])
        .unwrap();

        assert_eq!(composite.len(), 2);
        assert!(composite.get("p1").is_some());
        assert!(composite.get("p2").is_some());
    }

    #[test]
    fn composite_rejects_cross_registry_duplicate() {
        let mut r1 = InMemoryStopPolicyRegistry::new();
        r1.register_named("dup", Arc::new(MockStopPolicy::new("dup")))
            .unwrap();
        let mut r2 = InMemoryStopPolicyRegistry::new();
        r2.register_named("dup", Arc::new(MockStopPolicy::new("dup")))
            .unwrap();

        let err = CompositeStopPolicyRegistry::try_new(vec![
            Arc::new(r1) as Arc<dyn StopPolicyRegistry>,
            Arc::new(r2) as Arc<dyn StopPolicyRegistry>,
        ])
        .unwrap_err();
        assert!(matches!(
            err,
            StopPolicyRegistryError::StopPolicyIdConflict(ref id) if id == "dup"
        ));
    }
}
