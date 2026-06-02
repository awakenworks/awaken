use std::sync::Arc;

use awaken_runtime::registry::{
    AgentSpecRegistry, CompositeAgentSpecRegistry, MapAgentSpecRegistry, RemoteAgentSource,
};
use awaken_server_contract as server_contract;
use server_contract::A2aServerSpec;

use super::ConfigRuntimeManager;
use super::discovered_agents::{AgentSpecRegistryWithDiscovery, DiscoveredAgentRegistry};

impl ConfigRuntimeManager {
    pub(super) async fn discover_a2a_agents(
        &self,
        servers: &[A2aServerSpec],
    ) -> Option<Arc<dyn AgentSpecRegistry>> {
        if servers.is_empty() {
            return self.discovered_agents.clone();
        }

        let mut entries = Vec::new();
        for server in servers {
            let local = Arc::new(MapAgentSpecRegistry::new()) as Arc<dyn AgentSpecRegistry>;
            let mut composite = CompositeAgentSpecRegistry::new(local).with_local_name("config");
            composite.add_remote(RemoteAgentSource {
                name: server.id.clone(),
                base_url: server.base_url.clone(),
                bearer_token: server
                    .auth
                    .as_ref()
                    .and_then(|auth| auth.param_str("token").map(str::to_owned)),
            });

            match composite.discover().await {
                Ok(()) => {
                    for id in composite.agent_ids() {
                        if let Some(spec) = composite.get_agent(&id) {
                            entries.push((id, spec));
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        server_id = %server.id,
                        base_url = %server.base_url,
                        error = %error,
                        "A2A agent discovery failed; config publish will continue"
                    );
                }
            }
        }

        let discovered = DiscoveredAgentRegistry::from_entries(entries);
        match (discovered, &self.discovered_agents) {
            (Some(current), Some(existing)) => Some(Arc::new(AgentSpecRegistryWithDiscovery::new(
                current,
                Arc::clone(existing),
            )) as Arc<dyn AgentSpecRegistry>),
            (Some(current), None) => Some(current),
            (None, Some(existing)) => Some(Arc::clone(existing)),
            (None, None) => None,
        }
    }
}
