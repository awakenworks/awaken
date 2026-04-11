//! Versioned registry snapshots for runtime resolution.

use std::sync::Arc;

use parking_lot::RwLock;

use super::traits::RegistrySet;

#[derive(Clone)]
pub struct RegistrySnapshot {
    version: u64,
    registries: RegistrySet,
}

impl RegistrySnapshot {
    pub fn new(version: u64, registries: RegistrySet) -> Self {
        Self {
            version,
            registries,
        }
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn registries(&self) -> &RegistrySet {
        &self.registries
    }

    pub fn into_registries(self) -> RegistrySet {
        self.registries
    }
}

#[derive(Clone)]
pub struct RegistryHandle {
    snapshot: Arc<RwLock<RegistrySnapshot>>,
}

impl RegistryHandle {
    pub fn new(registries: RegistrySet) -> Self {
        Self {
            snapshot: Arc::new(RwLock::new(RegistrySnapshot::new(1, registries))),
        }
    }

    pub fn snapshot(&self) -> RegistrySnapshot {
        self.snapshot.read().clone()
    }

    pub fn version(&self) -> u64 {
        self.snapshot.read().version()
    }

    pub fn replace(&self, registries: RegistrySet) -> u64 {
        let mut snapshot = self.snapshot.write();
        let version = snapshot.version().saturating_add(1);
        *snapshot = RegistrySnapshot::new(version, registries);
        version
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::memory::{
        MapAgentSpecRegistry, MapBackendRegistry, MapModelRegistry, MapPluginSource,
        MapProviderRegistry, MapToolRegistry,
    };
    use awaken_contract::registry_spec::AgentSpec;

    fn make_registry_set(agent_id: &str) -> RegistrySet {
        let mut agents = MapAgentSpecRegistry::new();
        agents
            .register_spec(AgentSpec {
                id: agent_id.into(),
                model_id: "default".into(),
                system_prompt: "test".into(),
                ..Default::default()
            })
            .expect("register test agent");

        let mut models = MapModelRegistry::new();
        models
            .register_model(
                "default",
                crate::registry::ModelBinding {
                    provider_id: "provider".into(),
                    upstream_model: "gpt-test".into(),
                },
            )
            .expect("register test model");

        RegistrySet {
            agents: Arc::new(agents),
            tools: Arc::new(MapToolRegistry::new()),
            models: Arc::new(models),
            providers: Arc::new(MapProviderRegistry::new()),
            plugins: Arc::new(MapPluginSource::new()),
            backends: Arc::new(MapBackendRegistry::new()),
        }
    }

    #[test]
    fn new_starts_at_version_one() {
        let handle = RegistryHandle::new(make_registry_set("agent-a"));
        assert_eq!(handle.version(), 1);
        assert_eq!(
            handle.snapshot().registries().agents.agent_ids(),
            vec!["agent-a"]
        );
    }

    #[test]
    fn replace_publishes_new_version() {
        let handle = RegistryHandle::new(make_registry_set("agent-a"));
        let version = handle.replace(make_registry_set("agent-b"));
        assert_eq!(version, 2);
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.version(), 2);
        assert_eq!(snapshot.registries().agents.agent_ids(), vec!["agent-b"]);
    }
}
