//! Pure Kubernetes Pod security, resource, and network-posture projections.

use std::collections::BTreeMap;

use awaken_provisioning_contract as pc;
use k8s_openapi::api::core::v1::{Capabilities, PodSpec, ResourceRequirements, SecurityContext};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

/// The egress-posture label value an external platform policy may select on. The
/// label itself is metadata, not enforcement, so this adapter does not advertise
/// network isolation until composition can verify that policy separately.
pub(super) fn egress_label(network: &crate::NetworkMode) -> &'static str {
    match network {
        crate::NetworkMode::Open => "open",
        crate::NetworkMode::None => "restricted",
        crate::NetworkMode::Allowlist => "allowlist",
    }
}

/// Canonical labels selected by the composition-owned sandbox NetworkPolicies.
/// `None` selects only the default deny policy; a posture additionally selects
/// the matching egress policy.
pub(crate) fn sandbox_network_labels(posture: Option<&str>) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::from([("app".to_owned(), "awaken-sandbox".to_owned())]);
    if let Some(posture) = posture {
        labels.insert("awaken-egress".to_owned(), posture.to_owned());
    }
    labels
}

/// One shared predicate for Pod namespace shapes that invalidate a private
/// Sandbox boundary. `None`, explicit `false`, and an explicit empty ephemeral
/// container list are equivalent Kubernetes API-default representations.
pub(crate) fn has_forbidden_sandbox_namespace_shape(spec: &PodSpec) -> bool {
    spec.host_network == Some(true)
        || spec.host_pid == Some(true)
        || spec.host_ipc == Some(true)
        || spec.share_process_namespace == Some(true)
        || spec
            .ephemeral_containers
            .as_ref()
            .is_some_and(|containers| !containers.is_empty())
}

/// Admit the posture only when the Kubernetes composition has supplied exact
/// enforcement evidence. Labels alone never turn metadata into a boundary.
pub(super) fn admit_network(
    network: &crate::NetworkMode,
    restricted_policy: bool,
) -> Result<(), crate::RuntimeError> {
    if matches!(network, crate::NetworkMode::Open)
        || (matches!(
            network,
            crate::NetworkMode::None | crate::NetworkMode::Allowlist
        ) && restricted_policy)
    {
        Ok(())
    } else {
        Err(crate::RuntimeError::Backend(
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
        /* Cause/effect graph: C1 network is open, C2 the composition attests the
         * versioned restricted-egress policy. E1 admits Pod creation; E2 fails
         * before contacting Kubernetes. Decision rules: N1 C1+!C2=>E1;
         * N2 C1+C2=>E1; N3 !C1+C2=>E1; N4 !C1+!C2=>E2. Allowlist is rejected
         * earlier by the canonical egress planner and is not a NetworkMode. */
        assert!(
            admit_network(&crate::NetworkMode::Open, false).is_ok(),
            "N1"
        );
        assert!(admit_network(&crate::NetworkMode::Open, true).is_ok(), "N2");
        assert!(admit_network(&crate::NetworkMode::None, true).is_ok(), "N3");
        let error = admit_network(&crate::NetworkMode::None, false).expect_err("N4");
        assert!(error.to_string().contains("cannot prove"), "N4: {error}");
    }

    #[test]
    fn sandbox_namespace_predicate_rejects_only_boundary_weakening_shapes() {
        /* Namespace-shape decision table: C1=host network/PID/IPC requested;
         * C2=process namespace shared; C3=one ephemeral container injected;
         * C4=fields absent, explicitly false, and ephemeral list empty.
         * E1=C1|C2|C3 rejects the private Sandbox shape; E2=C4 remains
         * compatible with Kubernetes defaulting. Rules PS1 C1=>E1;
         * PS2 C2=>E1; PS3 C3=>E1; PS4 C4=>E2.
         */
        let mut compatible = PodSpec {
            containers: Vec::new(),
            host_network: Some(false),
            host_pid: Some(false),
            host_ipc: Some(false),
            share_process_namespace: Some(false),
            ephemeral_containers: Some(Vec::new()),
            ..Default::default()
        };
        assert!(
            !has_forbidden_sandbox_namespace_shape(&compatible),
            "PS4/E2"
        );

        let mutations: [crate::k8s::PodSpecMutation; 5] = [
            ("host network", |spec| spec.host_network = Some(true)),
            ("host PID", |spec| spec.host_pid = Some(true)),
            ("host IPC", |spec| spec.host_ipc = Some(true)),
            ("shared process namespace", |spec| {
                spec.share_process_namespace = Some(true)
            }),
            ("ephemeral container", |spec| {
                spec.ephemeral_containers = Some(vec![Default::default()])
            }),
        ];
        for (cause, mutate) in mutations {
            mutate(&mut compatible);
            assert!(
                has_forbidden_sandbox_namespace_shape(&compatible),
                "PS1-3/E1 {cause}"
            );
            compatible.host_network = Some(false);
            compatible.host_pid = Some(false);
            compatible.host_ipc = Some(false);
            compatible.share_process_namespace = Some(false);
            compatible.ephemeral_containers = Some(Vec::new());
        }
    }
}
