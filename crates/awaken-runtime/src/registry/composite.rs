//! Composite agent spec registry — combines local and remote agent discovery.
//!
//! Queries local agents first, then falls back to cached remote agents
//! discovered via the A2A agent card protocol.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use awaken_contract::contract::agent_card::AgentCard;
use awaken_contract::registry_spec::{AgentSpec, RemoteEndpoint};

use super::traits::AgentSpecRegistry;

// ---------------------------------------------------------------------------
// DiscoveryError
// ---------------------------------------------------------------------------

/// Errors from remote agent discovery.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("HTTP request failed for {url}: {message}")]
    HttpError { url: String, message: String },
    #[error("failed to decode agent card from {url}: {message}")]
    DecodeError { url: String, message: String },
}

// ---------------------------------------------------------------------------
// RemoteAgentSource
// ---------------------------------------------------------------------------

/// A source for remote agent discovery.
#[derive(Debug, Clone)]
pub struct RemoteAgentSource {
    /// Base URL of the remote A2A server.
    pub base_url: String,
    /// Optional bearer token for authentication.
    pub bearer_token: Option<String>,
}

// ---------------------------------------------------------------------------
// CompositeAgentSpecRegistry
// ---------------------------------------------------------------------------

/// Registry that combines local agents with remote agents discovered via A2A agent cards.
///
/// - Queries local registry first (always authoritative).
/// - Falls back to cached remote agent specs discovered via [`Self::discover`].
/// - Remote agents are converted from `AgentCard` to `AgentSpec` with the endpoint filled in.
pub struct CompositeAgentSpecRegistry {
    /// Local agent definitions (always queried first).
    local: Arc<dyn AgentSpecRegistry>,
    /// Remote A2A endpoints to discover agents from.
    remote_endpoints: Vec<RemoteAgentSource>,
    /// Cached remote agent specs keyed by agent ID.
    cache: RwLock<HashMap<String, AgentSpec>>,
    /// HTTP client for fetching agent cards.
    client: reqwest::Client,
}

impl CompositeAgentSpecRegistry {
    /// Create a new composite registry wrapping a local registry.
    pub fn new(local: Arc<dyn AgentSpecRegistry>) -> Self {
        Self {
            local,
            remote_endpoints: Vec::new(),
            cache: RwLock::new(HashMap::new()),
            client: reqwest::Client::new(),
        }
    }

    /// Add a remote endpoint to discover agents from.
    pub fn add_remote(&mut self, source: RemoteAgentSource) {
        self.remote_endpoints.push(source);
    }

    /// Discover agents from all remote endpoints.
    ///
    /// Fetches agent cards from `{base_url}/.well-known/agent.json`
    /// and converts them to `AgentSpec` with the endpoint filled in.
    /// Results are cached for subsequent lookups.
    pub async fn discover(&self) -> Result<(), DiscoveryError> {
        let mut new_cache = HashMap::new();

        for source in &self.remote_endpoints {
            let url = format!(
                "{}/.well-known/agent.json",
                source.base_url.trim_end_matches('/')
            );

            let mut request = self.client.get(&url);
            if let Some(ref token) = source.bearer_token {
                request = request.bearer_auth(token);
            }

            let response = request
                .send()
                .await
                .map_err(|e| DiscoveryError::HttpError {
                    url: url.clone(),
                    message: e.to_string(),
                })?;

            let response = response
                .error_for_status()
                .map_err(|e| DiscoveryError::HttpError {
                    url: url.clone(),
                    message: e.to_string(),
                })?;

            let card: AgentCard =
                response
                    .json()
                    .await
                    .map_err(|e| DiscoveryError::DecodeError {
                        url: url.clone(),
                        message: e.to_string(),
                    })?;

            let spec = agent_card_to_spec(&card, source);
            tracing::info!(
                agent_id = %spec.id,
                base_url = %source.base_url,
                "discovered remote agent"
            );
            new_cache.insert(spec.id.clone(), spec);
        }

        let mut cache = self.cache.write().expect("cache lock poisoned");
        *cache = new_cache;
        Ok(())
    }

    /// Manually refresh the remote agent cache (re-runs discovery).
    pub async fn refresh(&self) -> Result<(), DiscoveryError> {
        self.discover().await
    }
}

impl AgentSpecRegistry for CompositeAgentSpecRegistry {
    fn get_agent(&self, id: &str) -> Option<AgentSpec> {
        // Local registry is always authoritative.
        if let Some(spec) = self.local.get_agent(id) {
            return Some(spec);
        }

        // Fall back to cached remote agents.
        let cache = self.cache.read().expect("cache lock poisoned");
        cache.get(id).cloned()
    }

    fn agent_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.local.agent_ids();
        let cache = self.cache.read().expect("cache lock poisoned");
        for key in cache.keys() {
            if !ids.contains(key) {
                ids.push(key.clone());
            }
        }
        ids
    }
}

// ---------------------------------------------------------------------------
// Conversion: AgentCard → AgentSpec
// ---------------------------------------------------------------------------

/// Convert an A2A agent card into an `AgentSpec` with the remote endpoint configured.
fn agent_card_to_spec(card: &AgentCard, source: &RemoteAgentSource) -> AgentSpec {
    AgentSpec {
        id: card.id.clone(),
        // Remote agents don't need a local model — they run on the remote server.
        model: String::new(),
        system_prompt: card.description.clone(),
        endpoint: Some(RemoteEndpoint {
            base_url: card.url.clone(),
            bearer_token: source.bearer_token.clone(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::memory::MapAgentSpecRegistry;

    fn make_local_registry() -> Arc<dyn AgentSpecRegistry> {
        let mut reg = MapAgentSpecRegistry::new();
        reg.register(AgentSpec {
            id: "local-agent".into(),
            model: "test-model".into(),
            system_prompt: "Local agent.".into(),
            ..Default::default()
        });
        Arc::new(reg)
    }

    #[test]
    fn local_agent_lookup() {
        let composite = CompositeAgentSpecRegistry::new(make_local_registry());
        let spec = composite.get_agent("local-agent").unwrap();
        assert_eq!(spec.id, "local-agent");
        assert_eq!(spec.system_prompt, "Local agent.");
    }

    #[test]
    fn missing_agent_returns_none() {
        let composite = CompositeAgentSpecRegistry::new(make_local_registry());
        assert!(composite.get_agent("nonexistent").is_none());
    }

    #[test]
    fn agent_ids_includes_local() {
        let composite = CompositeAgentSpecRegistry::new(make_local_registry());
        let ids = composite.agent_ids();
        assert!(ids.contains(&"local-agent".to_string()));
    }

    #[test]
    fn cached_remote_agent_lookup() {
        let composite = CompositeAgentSpecRegistry::new(make_local_registry());

        // Manually populate cache to simulate discovery
        {
            let mut cache = composite.cache.write().unwrap();
            cache.insert(
                "remote-coder".into(),
                AgentSpec {
                    id: "remote-coder".into(),
                    model: String::new(),
                    system_prompt: "A remote coding agent.".into(),
                    endpoint: Some(RemoteEndpoint {
                        base_url: "https://remote.example.com".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            );
        }

        let spec = composite.get_agent("remote-coder").unwrap();
        assert_eq!(spec.id, "remote-coder");
        assert!(spec.endpoint.is_some());
    }

    #[test]
    fn local_takes_precedence_over_remote() {
        let composite = CompositeAgentSpecRegistry::new(make_local_registry());

        // Add a remote agent with the same ID as a local agent
        {
            let mut cache = composite.cache.write().unwrap();
            cache.insert(
                "local-agent".into(),
                AgentSpec {
                    id: "local-agent".into(),
                    model: String::new(),
                    system_prompt: "Remote version.".into(),
                    endpoint: Some(RemoteEndpoint {
                        base_url: "https://remote.example.com".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            );
        }

        // Local should take precedence
        let spec = composite.get_agent("local-agent").unwrap();
        assert_eq!(spec.system_prompt, "Local agent.");
        assert!(spec.endpoint.is_none());
    }

    #[test]
    fn agent_ids_includes_both_local_and_remote() {
        let composite = CompositeAgentSpecRegistry::new(make_local_registry());

        {
            let mut cache = composite.cache.write().unwrap();
            cache.insert(
                "remote-agent".into(),
                AgentSpec {
                    id: "remote-agent".into(),
                    ..Default::default()
                },
            );
        }

        let ids = composite.agent_ids();
        assert!(ids.contains(&"local-agent".to_string()));
        assert!(ids.contains(&"remote-agent".to_string()));
    }

    #[test]
    fn agent_card_to_spec_conversion() {
        let card = AgentCard {
            id: "test-agent".into(),
            name: "Test Agent".into(),
            description: "Handles tests.".into(),
            capabilities: vec!["testing".into()],
            url: "https://test.example.com".into(),
            auth: None,
        };
        let source = RemoteAgentSource {
            base_url: "https://test.example.com".into(),
            bearer_token: Some("tok-123".into()),
        };

        let spec = agent_card_to_spec(&card, &source);
        assert_eq!(spec.id, "test-agent");
        assert_eq!(spec.system_prompt, "Handles tests.");
        let endpoint = spec.endpoint.unwrap();
        assert_eq!(endpoint.base_url, "https://test.example.com");
        assert_eq!(endpoint.bearer_token.as_deref(), Some("tok-123"));
    }

    #[test]
    fn add_remote_sources() {
        let mut composite = CompositeAgentSpecRegistry::new(make_local_registry());
        composite.add_remote(RemoteAgentSource {
            base_url: "https://a.example.com".into(),
            bearer_token: None,
        });
        composite.add_remote(RemoteAgentSource {
            base_url: "https://b.example.com".into(),
            bearer_token: Some("tok".into()),
        });
        assert_eq!(composite.remote_endpoints.len(), 2);
    }

    #[test]
    fn discovery_error_display() {
        let err = DiscoveryError::HttpError {
            url: "https://example.com".into(),
            message: "connection refused".into(),
        };
        assert!(err.to_string().contains("connection refused"));

        let err = DiscoveryError::DecodeError {
            url: "https://example.com".into(),
            message: "invalid JSON".into(),
        };
        assert!(err.to_string().contains("invalid JSON"));
    }
}
