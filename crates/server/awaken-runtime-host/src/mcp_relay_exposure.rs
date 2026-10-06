//! Deployment-owned exposure of the Worker MCP relay to an isolated container
//! network: a validated bind/advertised-origin pair and the frozen-allowlist
//! admission check that keeps the relay the only route to an upstream target.

use std::net::{IpAddr, SocketAddr};

/// Deployment-owned exposure of the existing Worker MCP relay to an isolated
/// container network. The advertised origin carries only generation-scoped
/// synthetic route capabilities; credential material remains inside the
/// Runtime Host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerMcpRelayExposure {
    bind: SocketAddr,
    advertised_origin: url::Url,
}

fn is_loopback_address(address: IpAddr) -> bool {
    address.is_loopback()
        || match address {
            IpAddr::V4(_) => false,
            IpAddr::V6(address) => address
                .to_ipv4_mapped()
                .is_some_and(|mapped| mapped.is_loopback()),
        }
}

impl ContainerMcpRelayExposure {
    pub fn new(bind: SocketAddr, advertised_origin: impl AsRef<str>) -> Result<Self, String> {
        if bind.port() == 0 || is_loopback_address(bind.ip()) {
            return Err(
                "container MCP relay bind must use a non-loopback address and a fixed port".into(),
            );
        }
        let advertised_origin = url::Url::parse(advertised_origin.as_ref())
            .map_err(|error| format!("invalid container MCP relay URL: {error}"))?;
        if advertised_origin.scheme() != "http"
            || advertised_origin.host_str().is_none()
            || !advertised_origin.username().is_empty()
            || advertised_origin.password().is_some()
            || advertised_origin.path() != "/"
            || advertised_origin.query().is_some()
            || advertised_origin.fragment().is_some()
        {
            return Err(
                "container MCP relay URL must be an http origin without credentials, path, query, or fragment"
                    .into(),
            );
        }
        let unsafe_host = match advertised_origin
            .host()
            .expect("container MCP relay URL host checked above")
        {
            url::Host::Domain(host) => host.eq_ignore_ascii_case("localhost"),
            url::Host::Ipv4(address) => {
                is_loopback_address(IpAddr::V4(address)) || address.is_unspecified()
            }
            url::Host::Ipv6(address) => {
                is_loopback_address(IpAddr::V6(address)) || address.is_unspecified()
            }
        };
        if unsafe_host {
            return Err("container MCP relay URL must be reachable outside Worker loopback".into());
        }
        Ok(Self {
            bind,
            advertised_origin,
        })
    }

    #[must_use]
    pub const fn bind(&self) -> SocketAddr {
        self.bind
    }

    #[must_use]
    pub fn advertised_origin(&self) -> &url::Url {
        &self.advertised_origin
    }

    /// Require the frozen Environment allowlist to name the relay origin and to
    /// exclude the exact upstream target. The provider still owns enforcement;
    /// this check only prevents the frozen policy from explicitly authorizing a
    /// direct route around Worker mediation.
    #[must_use]
    pub fn admitted_by(
        &self,
        network: &awaken_provisioning_contract::NetworkPolicy,
        upstream_host: &str,
    ) -> bool {
        let awaken_provisioning_contract::NetworkPolicy::Allowlist { hosts } = network else {
            return false;
        };
        let expected = self
            .advertised_origin
            .host_str()
            .and_then(awaken_container_contract::normalize_allowlist_host);
        let upstream = awaken_container_contract::normalize_allowlist_host(upstream_host);
        expected.is_some_and(|expected| {
            let normalized = hosts
                .iter()
                .filter_map(|host| awaken_container_contract::normalize_allowlist_host(host))
                .collect::<Vec<_>>();
            normalized.iter().any(|candidate| candidate == &expected)
                && upstream.is_none_or(|upstream| {
                    normalized.iter().all(|candidate| candidate != &upstream)
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_relay_origin_requires_the_same_frozen_allowlist_host() {
        // Causes: C1 exposure is externally reachable and credential-free; C2
        // frozen Environment policy is Allowlist; C3 its normalized host set
        // contains the advertised relay host and excludes the exact upstream.
        // Effects: E1 C1 constructs the
        // endpoint; E2 C2+C3 admits mediated routing; E3 unrestricted, none,
        // upstream-only, malformed, and loopback coordinates reject. The
        // provider's independent live NetworkPolicy evidence is checked by the
        // existing capability conjunction and is not inferred here.
        let exposure = ContainerMcpRelayExposure::new(
            "0.0.0.0:8082".parse().unwrap(),
            "http://Relay.Awaken-Cloud.svc:8081",
        )
        .unwrap();
        let admitted = awaken_provisioning_contract::NetworkPolicy::Allowlist {
            hosts: vec!["relay.awaken-cloud.svc.".into()],
        };
        assert!(
            exposure.admitted_by(&admitted, "upstream.example.test"),
            "E2 normalized exact host"
        );
        for denied in [
            awaken_provisioning_contract::NetworkPolicy::Unrestricted,
            awaken_provisioning_contract::NetworkPolicy::None,
            awaken_provisioning_contract::NetworkPolicy::Allowlist {
                hosts: vec!["upstream.example.test".into()],
            },
        ] {
            assert!(
                !exposure.admitted_by(&denied, "upstream.example.test"),
                "E3 {denied:?}"
            );
        }
        let relay_and_upstream = awaken_provisioning_contract::NetworkPolicy::Allowlist {
            hosts: vec![
                "relay.awaken-cloud.svc".into(),
                "upstream.example.test".into(),
            ],
        };
        assert!(
            !exposure.admitted_by(&relay_and_upstream, "upstream.example.test"),
            "E3 the exact upstream target remains a direct bypass when also allowlisted"
        );
        for (bind, origin) in [
            ("127.0.0.1:8082", "http://relay.example.test:8082"),
            ("0.0.0.0:0", "http://relay.example.test:8082"),
            ("0.0.0.0:8082", "http://127.0.0.1:8082"),
            ("0.0.0.0:8082", "http://[::ffff:127.0.0.1]:8082"),
            ("0.0.0.0:8082", "https://relay.example.test:8082"),
            ("0.0.0.0:8082", "http://secret@relay.example.test:8082"),
            ("0.0.0.0:8082", "http://relay.example.test:8082/path"),
        ] {
            assert!(
                ContainerMcpRelayExposure::new(bind.parse().unwrap(), origin).is_err(),
                "E3 bind={bind} origin={origin}"
            );
        }
    }
}
