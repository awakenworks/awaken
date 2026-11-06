//! Deployment-owned container-tier policy values and the sandbox layout
//! coordinates a Session owner shares with every container provider.

use awaken_provisioning_contract as pc;

use std::collections::BTreeMap;

/// The Managed Sessions file subresource owns this stable projection root.
/// Kubernetes mounts it read-only into the Agent and read-write only into the
/// runtime-owned projector sidecar, which makes live replacement enforceable
/// without rebuilding the Session environment.
pub const LIVE_INPUTS_ROOT: &str = "/mnt/session/uploads";

/// Return a normalized path relative to [`LIVE_INPUTS_ROOT`]. This is the one
/// lexical admission rule shared by planning, live projection, removal, and Host
/// routing; callers must not reproduce prefix checks independently.
#[must_use]
pub fn live_input_relative_path(path: &str) -> Option<&str> {
    let relative = path.strip_prefix(LIVE_INPUTS_ROOT)?.strip_prefix('/')?;
    (!relative.is_empty()
        && relative
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".."))
    .then_some(relative)
}

/// Public internal-CA path mounted by the hosted Kubernetes Repository transport.
/// Only the exact Repository clone/push process opts into this trust root.
pub const REPOSITORY_GATEWAY_CA_PATH: &str = "/var/run/awaken/repository-trust/ca.crt";

/// Operator-authored limits for node image preparation; no Session authority.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct K8sImagePrewarm {
    pub max_images: usize,
    pub node_selector: BTreeMap<String, String>,
}

impl Default for K8sImagePrewarm {
    fn default() -> Self {
        Self {
            max_images: 16,
            node_selector: BTreeMap::new(),
        }
    }
}

impl K8sImagePrewarm {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=256).contains(&self.max_images) {
            return Err("image prewarm requires max_images 1..=256".into());
        }
        Ok(())
    }
}

/// CPU and memory reservation and ceiling for one Kubernetes Sandbox container
/// role. Requests make the Pod Burstable instead of BestEffort, so node memory
/// pressure no longer selects it first; limits bound one runaway container.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct K8sContainerBounds {
    pub cpu_request_millis: u64,
    pub memory_request_bytes: u64,
    pub cpu_limit_millis: u64,
    pub memory_limit_bytes: u64,
}

impl K8sContainerBounds {
    fn validate(&self, role: &str) -> Result<(), String> {
        if self.cpu_request_millis == 0 || self.memory_request_bytes == 0 {
            return Err(format!(
                "k8s_pod_bounds.{role} requires non-zero cpu and memory requests"
            ));
        }
        if self.cpu_request_millis > self.cpu_limit_millis
            || self.memory_request_bytes > self.memory_limit_bytes
        {
            return Err(format!(
                "k8s_pod_bounds.{role} requests must not exceed limits"
            ));
        }
        Ok(())
    }
}

/// Operator-authored bounds every hosted Kubernetes Sandbox Pod carries.
///
/// `agent` fills only the CPU/memory fields a Session plan leaves unset;
/// runtime-owned sidecars and init containers always carry `sidecar`. A
/// positive `ephemeral_active_deadline_secs` lets kubelet terminate a
/// disposable child/probe Pod whose owner never disposed it, releasing its
/// node capacity without granting anyone deletion authority. Retained Session
/// Pods are never deadline-bounded. Zero disables the deadline. Every emptyDir
/// volume without its own `sizeLimit` receives `empty_dir_size_limit_bytes`,
/// so kubelet evicts a runaway workspace before it fills the node disk; zero
/// disables that bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct K8sPodBounds {
    pub agent: K8sContainerBounds,
    pub sidecar: K8sContainerBounds,
    pub ephemeral_active_deadline_secs: u64,
    pub empty_dir_size_limit_bytes: u64,
}

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

impl Default for K8sPodBounds {
    fn default() -> Self {
        Self {
            agent: K8sContainerBounds {
                cpu_request_millis: 250,
                memory_request_bytes: 512 * MIB,
                cpu_limit_millis: 4_000,
                memory_limit_bytes: 8 * GIB,
            },
            sidecar: K8sContainerBounds {
                cpu_request_millis: 10,
                memory_request_bytes: 32 * MIB,
                cpu_limit_millis: 500,
                memory_limit_bytes: 256 * MIB,
            },
            ephemeral_active_deadline_secs: 6 * 60 * 60,
            empty_dir_size_limit_bytes: 20 * GIB,
        }
    }
}

impl K8sPodBounds {
    pub fn validate(&self) -> Result<(), String> {
        self.agent.validate("agent")?;
        self.sidecar.validate("sidecar")
    }
}

/// Deployment-owned policy for a Kubernetes Session's retained active volume.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct K8sContinuationVolume {
    pub storage_class_name: Option<String>,
    pub size: String,
}

impl K8sContinuationVolume {
    /// Shared admission for configured and directly constructed PVC policy.
    /// Kubernetes remains the authority for quantities and StorageClass existence.
    pub fn validate(&self) -> Result<(), pc::SandboxError> {
        if self.size.trim().is_empty() {
            return Err(pc::SandboxError::invalid(
                "k8s continuation volume size cannot be empty",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_image_preparation_policy() {
        // Ownership-only migration: all five policy values are imported from
        // this owner directly. It adds no runtime causes or effects, so the
        // existing P/L decision tables remain authoritative; affected
        // consumer feature builds verify removal of the forwarding paths.
        // Decision table P1 max_images at valid boundaries => admit;
        // P2 zero/above maximum => reject before Kubernetes effects.
        for max_images in [0, 1, 16, 256, 257] {
            let policy = K8sImagePrewarm {
                max_images,
                ..Default::default()
            };
            assert_eq!(policy.validate().is_ok(), (1..=256).contains(&max_images));
        }
    }

    #[test]
    fn pod_bounds_default_is_valid_and_rejects_unbounded_or_inverted_roles() {
        // Decision table B1 the shipped default admits; B2 a zero request (a
        // BestEffort role) rejects; B3 a request above its limit rejects; B4 a
        // partial authored table keeps the remaining defaults.
        assert!(K8sPodBounds::default().validate().is_ok(), "B1");
        let mut zero = K8sPodBounds::default();
        zero.sidecar.memory_request_bytes = 0;
        assert!(zero.validate().is_err(), "B2");
        let mut inverted = K8sPodBounds::default();
        inverted.agent.cpu_request_millis = inverted.agent.cpu_limit_millis + 1;
        assert!(inverted.validate().is_err(), "B3");
        let partial: K8sPodBounds =
            serde_json::from_str(r#"{"ephemeral_active_deadline_secs":0}"#).unwrap();
        assert_eq!(partial.agent, K8sPodBounds::default().agent, "B4");
        assert_eq!(partial.ephemeral_active_deadline_secs, 0, "B4");
    }

    #[test]
    fn live_input_paths_are_exact_children_of_the_projection_root() {
        // Decision table: L1 a normalized child => its relative path; L2 the
        // root itself, a sibling prefix, `.`/`..`/empty segments => None.
        assert_eq!(
            live_input_relative_path("/mnt/session/uploads/a/b.txt"),
            Some("a/b.txt"),
            "L1"
        );
        for rejected in [
            "/mnt/session/uploads",
            "/mnt/session/uploads/",
            "/mnt/session/uploadsx/a",
            "/mnt/session/uploads/../a",
            "/mnt/session/uploads/./a",
            "/mnt/session/uploads/a//b",
        ] {
            assert_eq!(live_input_relative_path(rejected), None, "L2 {rejected}");
        }
    }
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

/// Stable label every managed Sandbox container/pod carries. The wire value is
/// retained for compatibility with existing resources; it is discovery evidence,
/// never disposal authorization.
pub const MANAGED_SANDBOX_LABEL: &str = "awaken.sandbox";
