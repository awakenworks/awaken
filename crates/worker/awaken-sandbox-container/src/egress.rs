use awaken_provisioning_contract as pc;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// How egress maps onto a container network mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkMode {
    /// Full egress (default bridge).
    Open,
    /// No network.
    None,
    /// Direct egress is blocked by the runtime boundary; the only reachable
    /// public path is a capability-authenticated allowlist proxy.
    Allowlist,
}

/// An optional conventional forward proxy for unrestricted container traffic.
///
/// This is a connectivity preference, not a security boundary: arbitrary
/// workload code can remove proxy environment variables and dial directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardProxy {
    /// The forward-proxy URL cooperative clients use (e.g. `http://proxy:8888`).
    pub url: String,
}

/// Deployment-owned no-bypass proxy configuration.
///
/// The 32-byte key is mounted only into the Worker and Gateway. Each sandbox
/// receives a short-lived capability bound to its exact scope and normalized
/// host set; it never receives the signing key.
#[derive(Clone, PartialEq, Eq)]
pub struct AllowlistProxy {
    pub url: String,
    signing_key: [u8; 32],
    ttl_secs: u64,
}

impl std::fmt::Debug for AllowlistProxy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AllowlistProxy")
            .field("url", &self.url)
            .field("ttl_secs", &self.ttl_secs)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowlistCapability {
    pub version: u8,
    pub scope: String,
    pub hosts: Vec<String>,
    pub expires_at_unix: u64,
}

impl AllowlistProxy {
    pub fn new(
        url: impl Into<String>,
        signing_key: [u8; 32],
        ttl_secs: u64,
    ) -> Result<Self, EgressError> {
        let url = url.into();
        let authority = url
            .strip_prefix("http://")
            .filter(|authority| !authority.is_empty() && !authority.contains('@'))
            .ok_or(EgressError::InvalidAllowlistProxy)?;
        if authority.contains('/') || ttl_secs == 0 {
            return Err(EgressError::InvalidAllowlistProxy);
        }
        Ok(Self {
            url,
            signing_key,
            ttl_secs,
        })
    }

    pub fn issue_at(
        &self,
        scope: &str,
        hosts: &[String],
        now_unix: u64,
    ) -> Result<String, EgressError> {
        if scope.trim().is_empty() {
            return Err(EgressError::InvalidAllowlistScope);
        }
        let mut hosts = hosts
            .iter()
            .map(|host| normalize_hostname(host))
            .collect::<Result<Vec<_>, _>>()?;
        hosts.sort();
        hosts.dedup();
        if hosts.is_empty() {
            return Err(EgressError::EmptyAllowlist);
        }
        let capability = AllowlistCapability {
            version: 1,
            scope: scope.to_owned(),
            hosts,
            expires_at_unix: now_unix.saturating_add(self.ttl_secs),
        };
        let payload =
            serde_json::to_vec(&capability).map_err(|_| EgressError::InvalidAllowlistCapability)?;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
        let signature = blake3::keyed_hash(&self.signing_key, payload.as_bytes());
        let signature =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.as_bytes());
        let token = format!("v1.{payload}.{signature}");
        let authority = self
            .url
            .strip_prefix("http://")
            .expect("constructor validated proxy URL");
        Ok(format!("http://{token}:x@{authority}"))
    }

    fn issue(&self, scope: &str, hosts: &[String]) -> Result<String, EgressError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or_default();
        self.issue_at(scope, hosts, now)
    }
}

pub fn normalize_hostname(host: &str) -> Result<String, EgressError> {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty()
        || !host.is_ascii()
        || host.parse::<std::net::IpAddr>().is_ok()
        || host.len() > 253
    {
        return Err(EgressError::InvalidAllowlistHost(host));
    }
    if !host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    }) {
        return Err(EgressError::InvalidAllowlistHost(host));
    }
    Ok(host)
}

/// How one network policy is projected into a container plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressRealization {
    pub network: NetworkMode,
    /// `HTTPS_PROXY`/`HTTP_PROXY`/`NO_PROXY` pairs — empty when direct or denied.
    pub proxy_env: Vec<(String, String)>,
}

/// Why an egress policy cannot be realized on this tier.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EgressError {
    /// A process-level proxy cannot prove a no-bypass host allowlist.
    #[error("container provider has no no-bypass network allowlist enforcement")]
    AllowlistUnsupported,
    #[error("allowlist proxy configuration is invalid")]
    InvalidAllowlistProxy,
    #[error("allowlist capability scope is invalid")]
    InvalidAllowlistScope,
    #[error("network allowlist must contain at least one host")]
    EmptyAllowlist,
    #[error("network allowlist host is invalid: {0}")]
    InvalidAllowlistHost(String),
    #[error("allowlist capability could not be encoded")]
    InvalidAllowlistCapability,
}

/// Realize an egress policy for the container tier, fail-closed. A conventional
/// forward proxy may assist unrestricted connectivity, while `None` is enforced by
/// the container network mode. `Allowlist` is rejected until a provider supplies a
/// network boundary that blocks direct traffic; proxy environment variables alone
/// are deliberately insufficient.
pub fn egress_plan(
    policy: &pc::NetworkPolicy,
    proxy: Option<&ForwardProxy>,
) -> Result<EgressRealization, EgressError> {
    egress_plan_with_allowlist(policy, proxy, None, "")
}

pub fn egress_plan_with_allowlist(
    policy: &pc::NetworkPolicy,
    proxy: Option<&ForwardProxy>,
    allowlist_proxy: Option<&AllowlistProxy>,
    scope: &str,
) -> Result<EgressRealization, EgressError> {
    match policy {
        pc::NetworkPolicy::Unrestricted => Ok(EgressRealization {
            network: NetworkMode::Open,
            proxy_env: proxy.map(proxy_env).unwrap_or_default(),
        }),
        pc::NetworkPolicy::None => Ok(EgressRealization {
            network: NetworkMode::None,
            proxy_env: Vec::new(),
        }),
        pc::NetworkPolicy::Allowlist { hosts } => {
            let proxy = allowlist_proxy.ok_or(EgressError::AllowlistUnsupported)?;
            Ok(EgressRealization {
                network: NetworkMode::Allowlist,
                proxy_env: proxy_env_url(proxy.issue(scope, hosts)?),
            })
        }
    }
}

fn proxy_env(proxy: &ForwardProxy) -> Vec<(String, String)> {
    proxy_env_url(proxy.url.clone())
}

fn proxy_env_url(url: String) -> Vec<(String, String)> {
    vec![
        ("HTTPS_PROXY".to_string(), url.clone()),
        ("HTTP_PROXY".to_string(), url),
        ("NO_PROXY".to_string(), "localhost,127.0.0.1".to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Container-network cause graph:
    ///
    /// C1 unrestricted -> optional C2 forward proxy -> E1 open network (proxy
    /// env only when C2). C3 none -> E2 network none. C4 allowlist requires a
    /// capability issuer and is otherwise fail-closed.
    ///
    /// | Rule | Policy | Forward proxy | Network/result |
    /// |---|---|---:|---|
    /// | E1 | unrestricted | 0 | open, empty env |
    /// | E2 | unrestricted | 1 | open, proxy env |
    /// | E3 | none | - | none, empty env |
    /// | E4 | allowlist | conventional proxy only | unsupported |
    /// | E5 | allowlist | capability proxy | exact restricted plan |
    #[test]
    fn network_policy_decision_table() {
        let proxy = ForwardProxy {
            url: "http://gw.internal:8888".into(),
        };
        let open = egress_plan(&pc::NetworkPolicy::Unrestricted, None).unwrap();
        assert_eq!(open.network, NetworkMode::Open);
        assert!(open.proxy_env.is_empty());

        let proxied = egress_plan(&pc::NetworkPolicy::Unrestricted, Some(&proxy)).unwrap();
        assert_eq!(proxied.network, NetworkMode::Open);
        assert!(
            proxied
                .proxy_env
                .contains(&("HTTPS_PROXY".into(), proxy.url.clone()))
        );

        let denied = egress_plan(&pc::NetworkPolicy::None, Some(&proxy)).unwrap();
        assert_eq!(denied.network, NetworkMode::None);
        assert!(denied.proxy_env.is_empty());

        let allowlist = pc::NetworkPolicy::Allowlist {
            hosts: vec!["api.anthropic.com".into()],
        };
        assert_eq!(
            egress_plan(&allowlist, None),
            Err(EgressError::AllowlistUnsupported)
        );
        assert_eq!(
            egress_plan(&allowlist, Some(&proxy)),
            Err(EgressError::AllowlistUnsupported)
        );

        let allowlist_proxy =
            AllowlistProxy::new("http://gateway.internal:8081", [7; 32], 60).unwrap();
        let restricted =
            egress_plan_with_allowlist(&allowlist, None, Some(&allowlist_proxy), "session-a")
                .unwrap();
        assert_eq!(restricted.network, NetworkMode::Allowlist);
        let proxy_url = restricted
            .proxy_env
            .iter()
            .find(|(key, _)| key == "HTTPS_PROXY")
            .map(|(_, value)| value)
            .unwrap();
        assert!(proxy_url.starts_with("http://v1."));
        assert!(proxy_url.ends_with("@gateway.internal:8081"));
        assert!(!proxy_url.contains("api.anthropic.com"));
    }

    #[test]
    fn allowlist_capabilities_are_canonical_and_invalid_hosts_fail_closed() {
        let proxy = AllowlistProxy::new("http://gateway.internal:8081", [3; 32], 30).unwrap();
        let token = proxy
            .issue_at(
                "session-a",
                &[
                    "API.Example.COM.".into(),
                    "api.example.com".into(),
                    "cdn.example.com".into(),
                ],
                100,
            )
            .unwrap();
        let token = token
            .strip_prefix("http://")
            .unwrap()
            .split('@')
            .next()
            .unwrap()
            .strip_suffix(":x")
            .unwrap();
        assert_eq!(
            token,
            "v1.eyJ2ZXJzaW9uIjoxLCJzY29wZSI6InNlc3Npb24tYSIsImhvc3RzIjpbImFwaS5leGFtcGxlLmNvbSIsImNkbi5leGFtcGxlLmNvbSJdLCJleHBpcmVzX2F0X3VuaXgiOjEzMH0.XI59uSmnXkmB_BGB0uua5vGsPOxyduZD6yUiCLVxIvg"
        );
        let mut parts = token.split('.');
        assert_eq!(parts.next(), Some("v1"));
        let payload = parts.next().unwrap();
        assert!(parts.next().is_some());
        assert!(parts.next().is_none());
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .unwrap();
        let capability: AllowlistCapability = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(
            capability.hosts,
            ["api.example.com".to_owned(), "cdn.example.com".to_owned()]
        );
        assert_eq!(capability.expires_at_unix, 130);

        for invalid in ["127.0.0.1", "*.example.com", "-bad.example", "bad..example"] {
            assert!(matches!(
                proxy.issue_at("session-a", &[invalid.into()], 100),
                Err(EgressError::InvalidAllowlistHost(_))
            ));
        }
    }
}
