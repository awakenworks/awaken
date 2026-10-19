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
