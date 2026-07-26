use awaken_provisioning_contract as pc;

/// How egress maps onto a container network mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkMode {
    /// Full egress (default bridge).
    Open,
    /// No network.
    None,
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
    match policy {
        pc::NetworkPolicy::Unrestricted => Ok(EgressRealization {
            network: NetworkMode::Open,
            proxy_env: proxy.map(proxy_env).unwrap_or_default(),
        }),
        pc::NetworkPolicy::None => Ok(EgressRealization {
            network: NetworkMode::None,
            proxy_env: Vec::new(),
        }),
        pc::NetworkPolicy::Allowlist { .. } => Err(EgressError::AllowlistUnsupported),
    }
}

fn proxy_env(proxy: &ForwardProxy) -> Vec<(String, String)> {
    vec![
        ("HTTPS_PROXY".to_string(), proxy.url.clone()),
        ("HTTP_PROXY".to_string(), proxy.url.clone()),
        ("NO_PROXY".to_string(), "localhost,127.0.0.1".to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Container-network cause graph:
    ///
    /// C1 unrestricted -> optional C2 forward proxy -> E1 open network (proxy
    /// env only when C2). C3 none -> E2 network none. C4 allowlist always -> E3
    /// fail closed because C2 is not a no-bypass enforcement boundary.
    ///
    /// | Rule | Policy | Forward proxy | Network/result |
    /// |---|---|---:|---|
    /// | E1 | unrestricted | 0 | open, empty env |
    /// | E2 | unrestricted | 1 | open, proxy env |
    /// | E3 | none | - | none, empty env |
    /// | E4 | allowlist | 0 | unsupported |
    /// | E5 | allowlist | 1 | unsupported |
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
    }
}
