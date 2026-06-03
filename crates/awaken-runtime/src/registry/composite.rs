//! Composite agent spec registry — combines local and remote agent discovery.
//!
//! Queries local agents first, then falls back to cached remote agents
//! discovered via the A2A agent card protocol.
//!
//! Supports namespaced agent lookup: `"cloud/translator"` looks up agent
//! `"translator"` only in the `"cloud"` source, while `"analyst"` searches
//! all sources with local taking precedence.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use parking_lot::RwLock;

use awaken_protocol_a2a::{AgentCard, AgentInterface};
use awaken_runtime_contract::registry_spec::{
    A2A_SERVER_MAX_TIMEOUT_MS, AgentBackendSpec, AgentSpec, RemoteEndpoint, set_a2a_server_id,
};

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
    #[error(
        "remote agent card from {url} does not expose a supported HTTP+JSON v1.0 interface: {message}"
    )]
    UnsupportedInterface { url: String, message: String },
}

pub const A2A_DISCOVERY_RESPONSE_MAX_BYTES: usize = 256 * 1024;

#[must_use]
pub fn clamp_a2a_timeout_ms(timeout_ms: u64) -> u64 {
    timeout_ms.clamp(1, A2A_SERVER_MAX_TIMEOUT_MS)
}

fn a2a_http_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("A2A HTTP client must build")
        })
        .clone()
}

async fn validate_a2a_outbound_url(url: &reqwest::Url) -> Result<(), DiscoveryError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(DiscoveryError::HttpError {
            url: url.to_string(),
            message: "A2A discovery URL must use http or https".into(),
        });
    }
    let host = url.host_str().ok_or_else(|| DiscoveryError::HttpError {
        url: url.to_string(),
        message: "A2A discovery URL must include a host".into(),
    })?;
    if is_blocked_host_name(host) {
        return Err(DiscoveryError::HttpError {
            url: url.to_string(),
            message: "A2A discovery URL host is not allowed".into(),
        });
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        reject_private_ip(url, ip)?;
        return Ok(());
    }

    let port = url
        .port_or_known_default()
        .ok_or_else(|| DiscoveryError::HttpError {
            url: url.to_string(),
            message: "A2A discovery URL must include a resolvable port".into(),
        })?;
    let addresses = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| DiscoveryError::HttpError {
            url: url.to_string(),
            message: format!("failed to resolve A2A discovery host: {error}"),
        })?
        .collect::<Vec<SocketAddr>>();
    if addresses.is_empty() {
        return Err(DiscoveryError::HttpError {
            url: url.to_string(),
            message: "A2A discovery host resolved no addresses".into(),
        });
    }
    for address in addresses {
        reject_private_ip(url, address.ip())?;
    }
    Ok(())
}

fn is_blocked_host_name(host: &str) -> bool {
    let normalized = host.trim_end_matches('.').to_ascii_lowercase();
    normalized == "localhost" || normalized.ends_with(".localhost")
}

fn reject_private_ip(url: &reqwest::Url, ip: IpAddr) -> Result<(), DiscoveryError> {
    if is_private_or_special_ip(ip) {
        Err(DiscoveryError::HttpError {
            url: url.to_string(),
            message: format!("A2A discovery URL resolves to a non-public address: {ip}"),
        })
    } else {
        Ok(())
    }
}

fn is_private_or_special_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_private_or_special_ipv4(ip),
        IpAddr::V6(ip) => is_private_or_special_ipv6(ip),
    }
}

fn is_private_or_special_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || (224..=255).contains(&a)
}

fn is_private_or_special_ipv6(ip: Ipv6Addr) -> bool {
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.segments()[0] & 0xfe00 == 0xfc00
        || ip.segments()[0] & 0xffc0 == 0xfe80
        || ip.segments()[0] & 0xff00 == 0xff00
}

// ---------------------------------------------------------------------------
// RemoteAgentSource
// ---------------------------------------------------------------------------

/// A named source for remote agent discovery.
#[derive(Debug, Clone)]
pub struct RemoteAgentSource {
    /// Name of this registry source (e.g., "cloud", "internal", "partner").
    pub name: String,
    /// Endpoint defaults copied from the configured A2A server.
    pub endpoint: RemoteEndpoint,
}

impl RemoteAgentSource {
    #[must_use]
    pub fn from_endpoint(name: impl Into<String>, endpoint: RemoteEndpoint) -> Self {
        Self {
            name: name.into(),
            endpoint,
        }
    }

    fn bearer_token(&self) -> Option<&str> {
        self.endpoint
            .auth
            .as_ref()
            .and_then(|auth| auth.param_str("token"))
    }
}

// ---------------------------------------------------------------------------
// CompositeAgentSpecRegistry
// ---------------------------------------------------------------------------

/// Registry that combines local agents with remote agents discovered via A2A agent cards.
///
/// - Queries local registry first (always authoritative for plain IDs).
/// - Falls back to cached remote agent specs discovered via [`Self::discover`].
/// - Supports namespaced lookup: `"source/agent_id"` targets a specific source.
/// - Remote agents are converted from `AgentCard` to `AgentSpec` with the endpoint filled in.
pub struct CompositeAgentSpecRegistry {
    /// Name of the local registry source.
    local_name: String,
    /// Local agent definitions (always queried first for plain IDs).
    local: Arc<dyn AgentSpecRegistry>,
    /// Remote A2A endpoints to discover agents from.
    remote_endpoints: Vec<RemoteAgentSource>,
    /// Cached remote agent specs: agent_id → (source_name, AgentSpec).
    cache: RwLock<HashMap<String, (String, AgentSpec)>>,
}

impl CompositeAgentSpecRegistry {
    /// Create a new composite registry wrapping a local registry.
    pub fn new(local: Arc<dyn AgentSpecRegistry>) -> Self {
        Self {
            local_name: "local".to_string(),
            local,
            remote_endpoints: Vec::new(),
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// Create a new composite registry with a custom local source name.
    pub fn with_local_name(mut self, name: impl Into<String>) -> Self {
        self.local_name = name.into();
        self
    }

    /// Add a remote endpoint to discover agents from.
    pub fn add_remote(&mut self, source: RemoteAgentSource) {
        self.remote_endpoints.push(source);
    }

    /// Discover agents from all remote endpoints.
    ///
    /// Fetches agent cards from `/.well-known/agent-card.json` on the source's origin
    /// and converts them to `AgentSpec` with the endpoint filled in.
    /// Results are cached for subsequent lookups.
    pub async fn discover(&self) -> Result<(), DiscoveryError> {
        let mut new_cache: HashMap<String, (String, AgentSpec)> = HashMap::new();

        for source in &self.remote_endpoints {
            let (url, card) = fetch_a2a_agent_card(source).await?;
            let spec = agent_card_to_spec(&card, source, &url)?;
            tracing::info!(
                agent_id = %spec.id,
                source = %source.name,
                base_url = %source.endpoint.base_url,
                "discovered remote agent"
            );
            let cache_key = format!("{}/{}", source.name, spec.id);
            if let Some((existing_key, _)) = new_cache.iter().find(|(_, (_, s))| s.id == spec.id) {
                tracing::warn!(
                    agent_id = %spec.id,
                    existing_key = %existing_key,
                    new_source = %source.name,
                    "duplicate agent ID across sources — both entries are kept with namespaced keys"
                );
            }
            new_cache.insert(cache_key, (source.name.clone(), spec));
        }

        let mut cache = self.cache.write();
        *cache = new_cache;
        Ok(())
    }
}

pub async fn fetch_a2a_agent_card(
    source: &RemoteAgentSource,
) -> Result<(String, AgentCard), DiscoveryError> {
    fetch_a2a_agent_card_with_policy(source, true).await
}

async fn fetch_a2a_agent_card_with_policy(
    source: &RemoteAgentSource,
    enforce_url_policy: bool,
) -> Result<(String, AgentCard), DiscoveryError> {
    let url = a2a_discovery_url(&source.endpoint.base_url).map_err(|message| {
        DiscoveryError::HttpError {
            url: source.endpoint.base_url.clone(),
            message,
        }
    })?;
    let parsed_url = reqwest::Url::parse(&url).map_err(|error| DiscoveryError::HttpError {
        url: url.clone(),
        message: error.to_string(),
    })?;
    if enforce_url_policy {
        validate_a2a_outbound_url(&parsed_url).await?;
    }

    let mut request = a2a_http_client()
        .get(parsed_url)
        .timeout(Duration::from_millis(clamp_a2a_timeout_ms(
            source.endpoint.timeout_ms,
        )));
    if let Some(token) = source.bearer_token() {
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

    let card = decode_limited_agent_card(response, &url).await?;
    Ok((url, card))
}

async fn decode_limited_agent_card(
    mut response: reqwest::Response,
    url: &str,
) -> Result<AgentCard, DiscoveryError> {
    if response
        .content_length()
        .is_some_and(|len| len > A2A_DISCOVERY_RESPONSE_MAX_BYTES as u64)
    {
        return Err(DiscoveryError::DecodeError {
            url: url.to_string(),
            message: format!(
                "A2A agent card response exceeds {} bytes",
                A2A_DISCOVERY_RESPONSE_MAX_BYTES
            ),
        });
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| DiscoveryError::HttpError {
            url: url.to_string(),
            message: error.to_string(),
        })?
    {
        if body.len().saturating_add(chunk.len()) > A2A_DISCOVERY_RESPONSE_MAX_BYTES {
            return Err(DiscoveryError::DecodeError {
                url: url.to_string(),
                message: format!(
                    "A2A agent card response exceeds {} bytes",
                    A2A_DISCOVERY_RESPONSE_MAX_BYTES
                ),
            });
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice::<AgentCard>(&body).map_err(|error| DiscoveryError::DecodeError {
        url: url.to_string(),
        message: error.to_string(),
    })
}

impl AgentSpecRegistry for CompositeAgentSpecRegistry {
    fn get_agent(&self, id: &str) -> Option<AgentSpec> {
        // Check for namespaced ID: "source/agent_id"
        if let Some((source, agent_id)) = id.split_once('/') {
            if source == self.local_name {
                return self.local.get_agent(agent_id);
            }
            // Direct composite key lookup: "source/agent_id"
            let cache = self.cache.read();
            return cache.get(id).map(|(_, spec)| spec.clone());
        }

        // Plain ID: search local first, then all remote caches.
        if let Some(spec) = self.local.get_agent(id) {
            return Some(spec);
        }

        // Search all cached agents by agent ID
        let cache = self.cache.read();
        cache
            .iter()
            .find(|(_, (_, spec))| spec.id == id)
            .map(|(_, (_, spec))| spec.clone())
    }

    fn agent_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .local
            .agent_ids()
            .into_iter()
            .map(|id| format!("{}/{}", self.local_name, id))
            .collect();
        let cache = self.cache.read();
        for (key, _) in cache.iter() {
            ids.push(key.clone());
        }
        ids
    }
}

// ---------------------------------------------------------------------------
// Conversion: AgentCard → AgentSpec
// ---------------------------------------------------------------------------

/// Convert an A2A agent card into an `AgentSpec` with the remote endpoint configured.
fn agent_card_to_spec(
    card: &AgentCard,
    source: &RemoteAgentSource,
    discovery_url: &str,
) -> Result<AgentSpec, DiscoveryError> {
    let interface =
        select_supported_interface(card).ok_or_else(|| DiscoveryError::UnsupportedInterface {
            url: discovery_url.to_string(),
            message: format!(
                "supported interfaces were {:?}",
                card.supported_interfaces
                    .iter()
                    .map(|iface| format!("{} {}", iface.protocol_binding, iface.protocol_version))
                    .collect::<Vec<_>>()
            ),
        })?;

    let mut endpoint = source.endpoint.clone();
    endpoint.backend = "a2a".into();
    endpoint.base_url = interface.url.clone();
    endpoint.target = endpoint
        .target
        .clone()
        .or_else(|| interface.agent_id.clone());
    set_a2a_server_id(&mut endpoint, &source.name);

    Ok(AgentSpec {
        id: interface
            .agent_id
            .clone()
            .unwrap_or_else(|| slugify_agent_name(&card.name)),
        // Remote agents don't need a local model — they run on the remote server.
        model_id: String::new(),
        system_prompt: String::new(),
        description: Some(card.description.clone()),
        backend: AgentBackendSpec::from_remote_endpoint(&endpoint),
        endpoint: Some(endpoint),
        registry: Some(source.name.clone()),
        ..Default::default()
    })
}

fn select_supported_interface(card: &AgentCard) -> Option<&AgentInterface> {
    card.supported_interfaces
        .iter()
        .find(|iface| {
            iface.protocol_binding.eq_ignore_ascii_case("HTTP+JSON")
                && iface.protocol_version.trim() == "1.0"
        })
        .or_else(|| {
            card.supported_interfaces
                .iter()
                .find(|iface| iface.protocol_binding.eq_ignore_ascii_case("HTTP+JSON"))
        })
}

fn slugify_agent_name(name: &str) -> String {
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in name.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "agent".to_string()
    } else {
        slug.to_string()
    }
}

pub fn a2a_discovery_url(base_url: &str) -> Result<String, String> {
    let mut url = reqwest::Url::parse(base_url).map_err(|e| e.to_string())?;
    url.set_path("/.well-known/agent-card.json");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use awaken_runtime_contract::registry_spec::RemoteAuth;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::registry::memory::MapAgentSpecRegistry;

    fn make_local_registry() -> Arc<dyn AgentSpecRegistry> {
        let mut reg = MapAgentSpecRegistry::new();
        reg.register_spec(AgentSpec {
            id: "local-agent".into(),
            model_id: "test-model".into(),
            system_prompt: "Local agent.".into(),
            ..Default::default()
        })
        .unwrap();
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
    fn agent_ids_includes_local_namespaced() {
        let composite = CompositeAgentSpecRegistry::new(make_local_registry());
        let ids = composite.agent_ids();
        assert!(ids.contains(&"local/local-agent".to_string()));
    }

    #[test]
    fn cached_remote_agent_lookup() {
        let composite = CompositeAgentSpecRegistry::new(make_local_registry());

        // Manually populate cache to simulate discovery
        {
            let mut cache = composite.cache.write();
            cache.insert(
                "cloud/remote-coder".into(),
                (
                    "cloud".into(),
                    AgentSpec {
                        id: "remote-coder".into(),
                        model_id: String::new(),
                        system_prompt: "A remote coding agent.".into(),
                        endpoint: Some(RemoteEndpoint {
                            base_url: "https://remote.example.com".into(),
                            ..Default::default()
                        }),
                        registry: Some("cloud".into()),
                        ..Default::default()
                    },
                ),
            );
        }

        let spec = composite.get_agent("remote-coder").unwrap();
        assert_eq!(spec.id, "remote-coder");
        assert!(spec.endpoint.is_some());
        assert_eq!(spec.registry.as_deref(), Some("cloud"));
    }

    #[test]
    fn local_takes_precedence_over_remote() {
        let composite = CompositeAgentSpecRegistry::new(make_local_registry());

        // Add a remote agent with the same ID as a local agent
        {
            let mut cache = composite.cache.write();
            cache.insert(
                "cloud/local-agent".into(),
                (
                    "cloud".into(),
                    AgentSpec {
                        id: "local-agent".into(),
                        model_id: String::new(),
                        system_prompt: "Remote version.".into(),
                        endpoint: Some(RemoteEndpoint {
                            base_url: "https://remote.example.com".into(),
                            ..Default::default()
                        }),
                        registry: Some("cloud".into()),
                        ..Default::default()
                    },
                ),
            );
        }

        // Local should take precedence
        let spec = composite.get_agent("local-agent").unwrap();
        assert_eq!(spec.system_prompt, "Local agent.");
        assert!(spec.endpoint.is_none());
    }

    #[test]
    fn agent_ids_includes_both_local_and_remote_namespaced() {
        let composite = CompositeAgentSpecRegistry::new(make_local_registry());

        {
            let mut cache = composite.cache.write();
            cache.insert(
                "cloud/remote-agent".into(),
                (
                    "cloud".into(),
                    AgentSpec {
                        id: "remote-agent".into(),
                        ..Default::default()
                    },
                ),
            );
        }

        let ids = composite.agent_ids();
        assert!(ids.contains(&"local/local-agent".to_string()));
        assert!(ids.contains(&"cloud/remote-agent".to_string()));
    }

    #[test]
    fn agent_card_to_spec_conversion() {
        let card = AgentCard {
            name: "Test Agent".into(),
            description: "Handles tests.".into(),
            supported_interfaces: vec![AgentInterface {
                url: "https://test.example.com/v1/a2a".into(),
                protocol_binding: "HTTP+JSON".into(),
                protocol_version: "1.0".into(),
                agent_id: Some("test-agent".into()),
            }],
            provider: None,
            version: "1.0.0".into(),
            documentation_url: None,
            capabilities: awaken_protocol_a2a::AgentCapabilities::default(),
            security_schemes: std::collections::BTreeMap::new(),
            security: Vec::new(),
            default_input_modes: vec!["text/plain".into()],
            default_output_modes: vec!["text/plain".into()],
            skills: Vec::new(),
            signatures: Vec::new(),
            icon_url: None,
        };
        let mut options = std::collections::BTreeMap::new();
        options.insert("region".into(), serde_json::json!("us-east"));
        let source = RemoteAgentSource::from_endpoint(
            "cloud",
            RemoteEndpoint {
                base_url: "https://test.example.com".into(),
                auth: Some(RemoteAuth::bearer("tok-123")),
                target: Some("configured-target".into()),
                timeout_ms: 12_345,
                options,
                ..Default::default()
            },
        );

        let spec = agent_card_to_spec(
            &card,
            &source,
            "https://test.example.com/.well-known/agent-card.json",
        )
        .unwrap();
        assert_eq!(spec.id, "test-agent");
        assert_eq!(spec.description.as_deref(), Some("Handles tests."));
        assert_eq!(spec.system_prompt, "");
        assert_eq!(spec.backend.kind, "a2a");
        assert_eq!(spec.registry.as_deref(), Some("cloud"));
        let endpoint = spec.endpoint.unwrap();
        assert_eq!(endpoint.backend, "a2a");
        assert_eq!(endpoint.base_url, "https://test.example.com/v1/a2a");
        assert_eq!(endpoint.timeout_ms, 12_345);
        assert_eq!(
            endpoint
                .options
                .get("region")
                .and_then(serde_json::Value::as_str),
            Some("us-east")
        );
        assert_eq!(
            endpoint
                .auth
                .as_ref()
                .and_then(|auth| auth.param_str("token")),
            Some("tok-123")
        );
        assert_eq!(endpoint.target.as_deref(), Some("configured-target"));
    }

    #[test]
    fn add_remote_sources() {
        let mut composite = CompositeAgentSpecRegistry::new(make_local_registry());
        composite.add_remote(RemoteAgentSource::from_endpoint(
            "cloud",
            RemoteEndpoint {
                base_url: "https://a.example.com".into(),
                ..Default::default()
            },
        ));
        composite.add_remote(RemoteAgentSource::from_endpoint(
            "partner",
            RemoteEndpoint {
                base_url: "https://b.example.com".into(),
                auth: Some(RemoteAuth::bearer("tok")),
                ..Default::default()
            },
        ));
        assert_eq!(composite.remote_endpoints.len(), 2);
    }

    #[test]
    fn a2a_timeout_is_clamped_to_safe_bounds() {
        assert_eq!(clamp_a2a_timeout_ms(0), 1);
        assert_eq!(clamp_a2a_timeout_ms(1234), 1234);
        assert_eq!(
            clamp_a2a_timeout_ms(A2A_SERVER_MAX_TIMEOUT_MS + 1),
            A2A_SERVER_MAX_TIMEOUT_MS
        );
    }

    #[tokio::test]
    async fn a2a_outbound_policy_rejects_private_and_local_hosts() {
        for raw in [
            "http://localhost/.well-known/agent-card.json",
            "http://127.0.0.1/.well-known/agent-card.json",
            "http://169.254.169.254/latest/meta-data",
            "http://[::1]/.well-known/agent-card.json",
        ] {
            let url = reqwest::Url::parse(raw).expect("test URL parses");
            validate_a2a_outbound_url(&url)
                .await
                .expect_err("private/local A2A discovery URL must be rejected");
        }
    }

    #[tokio::test]
    async fn a2a_agent_card_decode_errors_are_reported_as_disconnected_status_inputs() {
        let base_url = serve_http_response(200, b"not-json".to_vec()).await;
        let source = RemoteAgentSource::from_endpoint(
            "local-test",
            RemoteEndpoint {
                base_url,
                timeout_ms: 1_000,
                ..Default::default()
            },
        );

        let err = fetch_a2a_agent_card_with_policy(&source, false)
            .await
            .expect_err("invalid JSON must not decode as an agent card");
        assert!(matches!(err, DiscoveryError::DecodeError { .. }));
    }

    #[tokio::test]
    async fn a2a_agent_card_response_size_is_limited() {
        let base_url =
            serve_http_response(200, vec![b'{'; A2A_DISCOVERY_RESPONSE_MAX_BYTES + 1]).await;
        let source = RemoteAgentSource::from_endpoint(
            "local-test",
            RemoteEndpoint {
                base_url,
                timeout_ms: 1_000,
                ..Default::default()
            },
        );

        let err = fetch_a2a_agent_card_with_policy(&source, false)
            .await
            .expect_err("oversized A2A agent cards must be rejected");
        assert!(err.to_string().contains("exceeds"));
    }

    async fn serve_http_response(status: u16, body: Vec<u8>) -> String {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind test HTTP listener");
        let address = listener.local_addr().expect("read listener address");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept HTTP request");
            let mut buffer = [0_u8; 1024];
            let _ = stream.read(&mut buffer).await.expect("read HTTP request");
            let response = format!(
                "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write HTTP headers");
            stream.write_all(&body).await.expect("write HTTP body");
        });
        format!("http://{address}/a2a")
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

        let err = DiscoveryError::UnsupportedInterface {
            url: "https://example.com".into(),
            message: "missing HTTP+JSON v1.0".into(),
        };
        assert!(err.to_string().contains("HTTP+JSON"));
    }

    #[test]
    fn discovery_url_uses_origin_root() {
        let url = a2a_discovery_url("https://api.example.com/v1/a2a").unwrap();
        assert_eq!(url, "https://api.example.com/.well-known/agent-card.json");
    }

    #[test]
    fn slugify_agent_name_produces_stable_id() {
        assert_eq!(slugify_agent_name("Remote Coder v2"), "remote-coder-v2");
        assert_eq!(slugify_agent_name("!!!"), "agent");
    }

    // -- Namespaced lookup tests --

    #[test]
    fn namespaced_lookup_local_source() {
        let composite = CompositeAgentSpecRegistry::new(make_local_registry());
        let spec = composite.get_agent("local/local-agent").unwrap();
        assert_eq!(spec.id, "local-agent");
        assert_eq!(spec.system_prompt, "Local agent.");
    }

    #[test]
    fn namespaced_lookup_remote_source() {
        let composite = CompositeAgentSpecRegistry::new(make_local_registry());

        {
            let mut cache = composite.cache.write();
            cache.insert(
                "cloud/translator".into(),
                (
                    "cloud".into(),
                    AgentSpec {
                        id: "translator".into(),
                        system_prompt: "Translates text.".into(),
                        registry: Some("cloud".into()),
                        ..Default::default()
                    },
                ),
            );
        }

        let spec = composite.get_agent("cloud/translator").unwrap();
        assert_eq!(spec.id, "translator");
        assert_eq!(spec.system_prompt, "Translates text.");
    }

    #[test]
    fn namespaced_lookup_wrong_source_returns_none() {
        let composite = CompositeAgentSpecRegistry::new(make_local_registry());

        {
            let mut cache = composite.cache.write();
            cache.insert(
                "cloud/translator".into(),
                (
                    "cloud".into(),
                    AgentSpec {
                        id: "translator".into(),
                        registry: Some("cloud".into()),
                        ..Default::default()
                    },
                ),
            );
        }

        // Agent exists in "cloud" but not in "partner"
        assert!(composite.get_agent("partner/translator").is_none());
    }

    #[test]
    fn namespaced_lookup_nonexistent_local_returns_none() {
        let composite = CompositeAgentSpecRegistry::new(make_local_registry());
        assert!(composite.get_agent("local/nonexistent").is_none());
    }

    #[test]
    fn custom_local_name() {
        let composite =
            CompositeAgentSpecRegistry::new(make_local_registry()).with_local_name("my-local");
        let ids = composite.agent_ids();
        assert!(ids.contains(&"my-local/local-agent".to_string()));

        // Namespaced lookup with custom local name
        let spec = composite.get_agent("my-local/local-agent").unwrap();
        assert_eq!(spec.id, "local-agent");
    }

    #[test]
    fn source_tracking_on_cached_agents() {
        let composite = CompositeAgentSpecRegistry::new(make_local_registry());

        {
            let mut cache = composite.cache.write();
            cache.insert(
                "partner/summarizer".into(),
                (
                    "partner".into(),
                    AgentSpec {
                        id: "summarizer".into(),
                        registry: Some("partner".into()),
                        ..Default::default()
                    },
                ),
            );
        }

        let spec = composite.get_agent("summarizer").unwrap();
        assert_eq!(spec.registry.as_deref(), Some("partner"));
    }

    #[test]
    fn multi_source_same_agent_id_both_kept() {
        let composite = CompositeAgentSpecRegistry::new(make_local_registry());

        {
            let mut cache = composite.cache.write();
            cache.insert(
                "cloud/translator".into(),
                (
                    "cloud".into(),
                    AgentSpec {
                        id: "translator".into(),
                        system_prompt: "Cloud translator.".into(),
                        registry: Some("cloud".into()),
                        ..Default::default()
                    },
                ),
            );
            cache.insert(
                "partner/translator".into(),
                (
                    "partner".into(),
                    AgentSpec {
                        id: "translator".into(),
                        system_prompt: "Partner translator.".into(),
                        registry: Some("partner".into()),
                        ..Default::default()
                    },
                ),
            );
        }

        // Namespaced lookups reach the correct source
        let cloud = composite.get_agent("cloud/translator").unwrap();
        assert_eq!(cloud.system_prompt, "Cloud translator.");

        let partner = composite.get_agent("partner/translator").unwrap();
        assert_eq!(partner.system_prompt, "Partner translator.");

        // Plain ID lookup returns one of them (non-deterministic order, but succeeds)
        let plain = composite.get_agent("translator");
        assert!(plain.is_some());

        // Both appear in agent_ids
        let ids = composite.agent_ids();
        assert!(ids.contains(&"cloud/translator".to_string()));
        assert!(ids.contains(&"partner/translator".to_string()));
    }

    #[test]
    fn agent_spec_registry_field_serialization() {
        let spec = AgentSpec {
            id: "test".into(),
            registry: Some("cloud".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&spec).unwrap();
        assert!(json.contains("\"registry\":\"cloud\""));

        let parsed: AgentSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.registry.as_deref(), Some("cloud"));
    }

    #[test]
    fn agent_spec_registry_field_skipped_when_none() {
        let spec = AgentSpec {
            id: "test".into(),
            ..Default::default()
        };
        let json = serde_json::to_string(&spec).unwrap();
        assert!(!json.contains("registry"));
    }
}
