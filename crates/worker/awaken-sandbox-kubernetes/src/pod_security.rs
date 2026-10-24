//! Pure Kubernetes Pod security, resource, and network-posture projections.

use std::collections::BTreeMap;

use awaken_provisioning_contract as pc;
use k8s_openapi::api::core::v1::{
    Capabilities, ResourceRequirements, SeccompProfile, SecurityContext,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

/// The egress-posture label value an external platform policy may select on. The
/// label itself is metadata, not enforcement, so this adapter does not advertise
/// network isolation until composition can verify that policy separately.
pub(super) fn egress_label(network: &awaken_sandbox_container::NetworkMode) -> &'static str {
    match network {
        awaken_sandbox_container::NetworkMode::Open => "open",
        awaken_sandbox_container::NetworkMode::None => "restricted",
        awaken_sandbox_container::NetworkMode::Allowlist => "allowlist",
    }
}

/// Admit the posture only when the Kubernetes composition has supplied exact
/// enforcement evidence. Labels alone never turn metadata into a boundary.
pub(super) fn admit_network(
    network: &awaken_sandbox_container::NetworkMode,
    restricted_policy: bool,
    allowlist_policy: bool,
) -> Result<(), awaken_container_contract::RuntimeError> {
    let admitted = match network {
        awaken_sandbox_container::NetworkMode::Open => true,
        awaken_sandbox_container::NetworkMode::None => restricted_policy,
        awaken_sandbox_container::NetworkMode::Allowlist => allowlist_policy,
    };
    if admitted {
        Ok(())
    } else {
        Err(awaken_container_contract::RuntimeError::Backend(
            "k8s adapter cannot prove an installed network-isolation policy".into(),
        ))
    }
}

/// The hardened `securityContext` for the untrusted agent container: no privilege
/// escalation, every Linux capability dropped.
pub(super) fn hardened_security_context() -> SecurityContext {
    SecurityContext {
        allow_privilege_escalation: Some(false),
        read_only_root_filesystem: Some(true),
        run_as_non_root: Some(true),
        run_as_user: Some(awaken_sandbox_container::engine::SANDBOX_EXECUTION_ID),
        run_as_group: Some(awaken_sandbox_container::engine::SANDBOX_EXECUTION_ID),
        seccomp_profile: Some(SeccompProfile {
            type_: "RuntimeDefault".into(),
            localhost_profile: None,
        }),
        capabilities: Some(Capabilities {
            drop: Some(vec!["ALL".to_string()]),
            add: None,
        }),
        ..Default::default()
    }
}

/// The Pod container's native CPU, memory, and ephemeral-storage reservations
/// and limits. Kubernetes scheduling consumes requests; cgroups enforce limits.
pub(super) fn pod_resources(
    requests: &pc::ResourceRequests,
    limits: &pc::ResourceLimits,
) -> Option<ResourceRequirements> {
    if !requests.is_set() && !limits.is_set() {
        return None;
    }
    let mut projected_requests = BTreeMap::new();
    if let Some(cpu) = requests.cpu_millis {
        projected_requests.insert("cpu".to_string(), Quantity(format!("{cpu}m")));
    }
    if let Some(memory) = requests.memory_bytes {
        projected_requests.insert("memory".to_string(), Quantity(memory.to_string()));
    }
    if let Some(disk) = requests.disk_bytes {
        projected_requests.insert("ephemeral-storage".to_string(), Quantity(disk.to_string()));
    }
    let mut projected_limits = BTreeMap::new();
    if let Some(cpu) = limits.cpu_millis {
        projected_limits.insert("cpu".to_string(), Quantity(format!("{cpu}m")));
    }
    if let Some(memory) = limits.memory_bytes {
        projected_limits.insert("memory".to_string(), Quantity(memory.to_string()));
    }
    if let Some(disk) = limits.disk_bytes {
        projected_limits.insert("ephemeral-storage".to_string(), Quantity(disk.to_string()));
    }
    if projected_requests.is_empty() && projected_limits.is_empty() {
        return None;
    }
    Some(ResourceRequirements {
        requests: (!projected_requests.is_empty()).then_some(projected_requests),
        limits: (!projected_limits.is_empty()).then_some(projected_limits),
        claims: None,
    })
}

/// Return the requested limit Kubernetes cannot express at Pod-spec level.
pub(super) fn unenforceable_k8s_limit(limits: &pc::ResourceLimits) -> Option<&'static str> {
    limits.pids.map(|_| "pids")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restricted_network_requires_exact_composition_evidence() {
        // N1 Open admits independently of both evidence bits. N2 None admits
        // exactly on deny-all evidence. N3 Allowlist admits exactly on its own
        // proxy-only evidence: neither posture may borrow the other's proof.
        // The issuer remains an independent earlier capability predicate.
        for network in [
            awaken_sandbox_container::NetworkMode::Open,
            awaken_sandbox_container::NetworkMode::None,
            awaken_sandbox_container::NetworkMode::Allowlist,
        ] {
            for restricted in [false, true] {
                for allowlist in [false, true] {
                    let expected = match network {
                        awaken_sandbox_container::NetworkMode::Open => true,
                        awaken_sandbox_container::NetworkMode::None => restricted,
                        awaken_sandbox_container::NetworkMode::Allowlist => allowlist,
                    };
                    let result = admit_network(&network, restricted, allowlist);
                    assert_eq!(
                        result.is_ok(),
                        expected,
                        "N1-N3 {network:?} {restricted}/{allowlist}"
                    );
                    if let Err(error) = result {
                        assert!(error.to_string().contains("cannot prove"));
                    }
                }
            }
        }
    }
}
