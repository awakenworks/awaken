//! Pure Kubernetes Pod security, resource, and network-posture projections.

use std::collections::BTreeMap;

use awaken_container_contract::{K8sContainerBounds, K8sPodBounds};
use awaken_provisioning_contract as pc;
use awaken_worker_fleet_kubernetes::pod_resources::{
    byte_quantity, cpu_quantity, quantity_bytes, quantity_cpu_millis,
};
use k8s_openapi::api::core::v1::{Capabilities, Container, Pod, SeccompProfile, SecurityContext};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;

/// The untrusted workload container; every other container is runtime-owned.
const AGENT_CONTAINER: &str = "agent";

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

/// Give every container of one hosted Sandbox Pod a CPU/memory reservation and
/// ceiling, and bound a disposable Pod's lifetime.
///
/// A Session plan's own values win field by field; the operator bounds fill
/// only what it leaves unset. A plan ceiling below the default reservation
/// lowers the reservation, and a plan reservation above the default ceiling
/// raises the ceiling, because Kubernetes rejects a request above its limit.
/// Container ephemeral-storage stays exactly as the plan declares it; every
/// emptyDir without its own `sizeLimit` receives the operator volume bound, so
/// one runaway workspace is evicted instead of filling the node disk.
pub(super) fn apply_pod_bounds(
    pod: &mut Pod,
    continuity: pc::FilesystemContinuity,
    bounds: &K8sPodBounds,
) {
    let Some(spec) = pod.spec.as_mut() else {
        return;
    };
    for container in &mut spec.containers {
        let role = if container.name == AGENT_CONTAINER {
            &bounds.agent
        } else {
            &bounds.sidecar
        };
        bound_container(container, role);
    }
    for container in spec.init_containers.iter_mut().flatten() {
        bound_container(container, &bounds.sidecar);
    }
    if bounds.empty_dir_size_limit_bytes > 0 {
        let limit = byte_quantity(bounds.empty_dir_size_limit_bytes);
        for empty_dir in spec
            .volumes
            .iter_mut()
            .flatten()
            .filter_map(|volume| volume.empty_dir.as_mut())
        {
            empty_dir.size_limit.get_or_insert_with(|| limit.clone());
        }
    }
    if continuity == pc::FilesystemContinuity::Ephemeral
        && bounds.ephemeral_active_deadline_secs > 0
    {
        spec.active_deadline_seconds =
            Some(i64::try_from(bounds.ephemeral_active_deadline_secs).unwrap_or(i64::MAX));
    }
}

fn bound_container(container: &mut Container, bounds: &K8sContainerBounds) {
    let resources = container.resources.get_or_insert_with(Default::default);
    let requests = resources.requests.get_or_insert_with(Default::default);
    let limits = resources.limits.get_or_insert_with(Default::default);
    fill_pair(
        requests,
        limits,
        "cpu",
        (bounds.cpu_request_millis, bounds.cpu_limit_millis),
        quantity_cpu_millis,
        cpu_quantity,
    );
    fill_pair(
        requests,
        limits,
        "memory",
        (bounds.memory_request_bytes, bounds.memory_limit_bytes),
        quantity_bytes,
        byte_quantity,
    );
}

fn fill_pair(
    requests: &mut BTreeMap<String, Quantity>,
    limits: &mut BTreeMap<String, Quantity>,
    key: &str,
    (default_request, default_limit): (u64, u64),
    parse: fn(&Quantity) -> Option<u64>,
    render: fn(u64) -> Quantity,
) {
    if !limits.contains_key(key) {
        let declared_request = requests.get(key).and_then(parse).unwrap_or(0);
        limits.insert(key.to_string(), render(default_limit.max(declared_request)));
    }
    if !requests.contains_key(key) {
        let ceiling = limits.get(key).and_then(parse).unwrap_or(u64::MAX);
        requests.insert(key.to_string(), render(default_request.min(ceiling)));
    }
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
