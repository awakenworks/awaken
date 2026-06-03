use std::collections::HashMap;
use std::sync::Arc;

use awaken_runtime::registry::AgentSpecRegistry;
use awaken_server_contract as server_contract;
use server_contract::AgentSpec;

#[derive(Default)]
pub(super) struct DiscoveredAgentRegistry {
    exact: HashMap<String, AgentSpec>,
    plain: HashMap<String, AgentSpec>,
}

impl DiscoveredAgentRegistry {
    pub(super) fn from_registry(
        registry: Arc<dyn AgentSpecRegistry>,
    ) -> Option<Arc<dyn AgentSpecRegistry>> {
        let mut exact = HashMap::new();
        let mut plain = HashMap::new();

        for id in registry.agent_ids() {
            let Some(spec) = registry.get_agent(&id) else {
                continue;
            };
            if spec.endpoint.is_none() && spec.registry.is_none() {
                continue;
            }
            plain.entry(spec.id.clone()).or_insert_with(|| spec.clone());
            exact.insert(id, spec);
        }

        if exact.is_empty() {
            None
        } else {
            Some(Arc::new(Self { exact, plain }) as Arc<dyn AgentSpecRegistry>)
        }
    }

    pub(super) fn from_entries(
        entries: impl IntoIterator<Item = (String, AgentSpec)>,
    ) -> Option<Arc<dyn AgentSpecRegistry>> {
        let mut exact = HashMap::new();
        let mut plain: HashMap<String, AgentSpec> = HashMap::new();

        for (id, spec) in entries {
            if let Some(existing) = plain.get(&spec.id) {
                tracing::warn!(
                    agent_id = %spec.id,
                    existing_registry = ?existing.registry,
                    duplicate_registry = ?spec.registry,
                    namespaced_id = %id,
                    "duplicate discovered A2A agent plain id; first plain lookup wins, use namespaced id to disambiguate"
                );
            } else {
                plain.insert(spec.id.clone(), spec.clone());
            }
            exact.insert(id, spec);
        }

        if exact.is_empty() {
            None
        } else {
            Some(Arc::new(Self { exact, plain }) as Arc<dyn AgentSpecRegistry>)
        }
    }
}

impl AgentSpecRegistry for DiscoveredAgentRegistry {
    fn get_agent(&self, id: &str) -> Option<AgentSpec> {
        self.exact
            .get(id)
            .cloned()
            .or_else(|| self.plain.get(id).cloned())
    }

    fn agent_ids(&self) -> Vec<String> {
        let mut ids: Vec<_> = self.exact.keys().cloned().collect();
        ids.sort();
        ids
    }
}

pub(super) struct AgentSpecRegistryWithDiscovery {
    base: Arc<dyn AgentSpecRegistry>,
    overlay: Arc<dyn AgentSpecRegistry>,
}

impl AgentSpecRegistryWithDiscovery {
    pub(super) fn new(
        base: Arc<dyn AgentSpecRegistry>,
        overlay: Arc<dyn AgentSpecRegistry>,
    ) -> Self {
        Self { base, overlay }
    }
}

impl AgentSpecRegistry for AgentSpecRegistryWithDiscovery {
    fn get_agent(&self, id: &str) -> Option<AgentSpec> {
        self.base
            .get_agent(id)
            .or_else(|| self.overlay.get_agent(id))
    }

    fn agent_ids(&self) -> Vec<String> {
        let mut ids = self.base.agent_ids();
        ids.extend(self.overlay.agent_ids());
        ids.sort();
        ids.dedup();
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_plain_agent_id_keeps_namespaced_entries_and_first_plain_lookup() {
        let registry = DiscoveredAgentRegistry::from_entries([
            (
                "first/assistant".to_string(),
                AgentSpec {
                    id: "assistant".to_string(),
                    registry: Some("first".to_string()),
                    ..Default::default()
                },
            ),
            (
                "second/assistant".to_string(),
                AgentSpec {
                    id: "assistant".to_string(),
                    registry: Some("second".to_string()),
                    ..Default::default()
                },
            ),
        ])
        .expect("registry should be built");

        assert_eq!(
            registry
                .get_agent("assistant")
                .and_then(|spec| spec.registry),
            Some("first".to_string())
        );
        assert_eq!(
            registry
                .get_agent("second/assistant")
                .and_then(|spec| spec.registry),
            Some("second".to_string())
        );
        assert_eq!(
            registry.agent_ids(),
            vec![
                "first/assistant".to_string(),
                "second/assistant".to_string()
            ]
        );
    }
}
