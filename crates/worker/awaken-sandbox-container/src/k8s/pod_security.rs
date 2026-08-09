//! Pure Kubernetes Pod security, resource, and network-posture projections.

use std::collections::BTreeMap;

use awaken_provisioning_contract as pc;
use k8s_openapi::api::core::v1::{Capabilities, ResourceRequirements, SecurityContext};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

/// The egress-posture label value an external platform policy may select on. The
/// label itself is metadata, not enforcement, so this adapter does not advertise
/// network isolation until composition can verify that policy separately.
pub(super) fn egress_label(network: &crate::NetworkMode) -> &'static str {
    match network {
        crate::NetworkMode::Open => "open",
        crate::NetworkMode::None => "restricted",
    }
}

/// The hardened `securityContext` for the untrusted agent container: no privilege
/// escalation, every Linux capability dropped.
pub(super) fn hardened_security_context() -> SecurityContext {
    SecurityContext {
        allow_privilege_escalation: Some(false),
        read_only_root_filesystem: Some(true),
        capabilities: Some(Capabilities {
            drop: Some(vec!["ALL".to_string()]),
            add: None,
        }),
        ..Default::default()
    }
}

/// The Pod container's native CPU, memory, and ephemeral-storage limits.
pub(super) fn pod_resources(limits: &pc::ResourceLimits) -> Option<ResourceRequirements> {
    if !limits.is_set() {
        return None;
    }
    let mut projected = BTreeMap::new();
    if let Some(cpu) = limits.cpu_millis {
        projected.insert("cpu".to_string(), Quantity(format!("{cpu}m")));
    }
    if let Some(memory) = limits.memory_bytes {
        projected.insert("memory".to_string(), Quantity(memory.to_string()));
    }
    if let Some(disk) = limits.disk_bytes {
        projected.insert("ephemeral-storage".to_string(), Quantity(disk.to_string()));
    }
    if projected.is_empty() {
        return None;
    }
    Some(ResourceRequirements {
        limits: Some(projected),
        ..Default::default()
    })
}

/// Return the requested limit Kubernetes cannot express at Pod-spec level.
pub(super) fn unenforceable_k8s_limit(limits: &pc::ResourceLimits) -> Option<&'static str> {
    limits.pids.map(|_| "pids")
}
