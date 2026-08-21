use std::path::{Component, Path};

use awaken_provisioning_contract as pc;

use super::ContainerPlan;

/// Runtime-neutral storage topology for one canonical writable root.
///
/// Kubernetes consumes the retained branch as one PVC plus a distinct subpath;
/// Docker/Podman and the Kubernetes fallback consume the ephemeral branch as
/// one independent writable volume per root. Keeping this relation outside the
/// Kubernetes adapter makes the no-aliasing rule directly model-checkable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(any(feature = "k8s", test, kani))]
pub(crate) enum WritableRootTopology {
    Retained {
        claim_slot: usize,
        subpath_slot: usize,
    },
    Ephemeral {
        volume_slot: usize,
    },
}

#[must_use]
#[cfg(any(feature = "k8s", test, kani))]
pub(crate) const fn writable_root_topology(
    retained_claim: bool,
    root_index: usize,
) -> WritableRootTopology {
    if retained_claim {
        WritableRootTopology::Retained {
            claim_slot: 0,
            subpath_slot: root_index,
        }
    } else {
        WritableRootTopology::Ephemeral {
            volume_slot: root_index,
        }
    }
}

#[cfg(kani)]
#[kani::proof]
fn continuation_writable_roots_share_one_claim_without_aliasing() {
    let first: usize = kani::any();
    let second: usize = kani::any();
    kani::assume(first != second);

    let (
        WritableRootTopology::Retained {
            claim_slot: first_claim,
            subpath_slot: first_subpath,
        },
        WritableRootTopology::Retained {
            claim_slot: second_claim,
            subpath_slot: second_subpath,
        },
    ) = (
        writable_root_topology(true, first),
        writable_root_topology(true, second),
    )
    else {
        unreachable!();
    };
    assert_eq!(first_claim, second_claim);
    assert_ne!(first_subpath, second_subpath);

    let (
        WritableRootTopology::Ephemeral {
            volume_slot: first_volume,
        },
        WritableRootTopology::Ephemeral {
            volume_slot: second_volume,
        },
    ) = (
        writable_root_topology(false, first),
        writable_root_topology(false, second),
    )
    else {
        unreachable!();
    };
    assert_ne!(first_volume, second_volume);
}

fn bounded_absolute(path: &str) -> bool {
    !path.is_empty()
        && path != "/"
        && Path::new(path).is_absolute()
        && !Path::new(path)
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::Prefix(_)))
}

/// Canonical archive roots for one Sandbox specification. This is the single
/// source consumed by product checkpoint decorators; runtime-specific code must
/// not restate the workspace/output/tmp set.
pub fn checkpoint_writable_roots(spec: &pc::SandboxSpec) -> Result<Vec<String>, pc::SandboxError> {
    let output = spec.outputs_path.trim_end_matches('/');
    if !bounded_absolute(output) {
        return Err(pc::SandboxError::new(
            "sandbox output root must be an absolute bounded path",
        ));
    }
    let mut roots = vec!["/workspace".to_owned()];
    if !Path::new(output).starts_with("/workspace") && output != "/tmp" {
        roots.push(output.to_owned());
    }
    roots.push("/tmp".to_owned());
    Ok(roots)
}

/// Revalidate roots recovered from a durable handle before they can influence
/// an archive command. The third root, when present, is the exact output root.
pub fn validate_checkpoint_writable_roots(roots: &[String]) -> Result<(), pc::SandboxError> {
    let valid_shape = match roots {
        [workspace, tmp] => workspace == "/workspace" && tmp == "/tmp",
        [workspace, output, tmp] => {
            workspace == "/workspace"
                && tmp == "/tmp"
                && bounded_absolute(output)
                && !Path::new(output).starts_with("/workspace")
                && output != "/tmp"
        }
        _ => false,
    };
    if !valid_shape {
        return Err(pc::SandboxError::new(
            "persisted sandbox writable roots are not canonical",
        ));
    }
    Ok(())
}

/// The sandbox paths that must stay writable under a **read-only rootfs**: the
/// Session workspace, the outputs volume the agent writes artifacts to, and a
/// scratch `/tmp`. Declared resource mounts are realized separately (as
/// binds/volumes). Pure, so every adapter renders the same writable set atop the
/// same hardening.
#[must_use]
pub fn writable_dirs(plan: &ContainerPlan) -> Vec<String> {
    let mut dirs = vec!["/workspace".to_string()];
    // OCI creates the parent of a file bind as root:root. Keep the parent of every
    // host-materialized workspace file on a Session-private writable volume so the
    // non-root Agent can later attach, rename, or detach sibling resources without
    // granting it root. Directory binds (repositories/memory) remain governed by
    // their own mount and must not be shadowed here.
    for bind in &plan.binds {
        let is_file =
            bind.content.is_some() || bind.content_bytes.is_some() || bind.secret_content.is_some();
        let parent = is_file
            .then(|| std::path::Path::new(&bind.mount_path).parent())
            .flatten()
            .and_then(std::path::Path::to_str)
            .filter(|parent| parent.starts_with("/workspace/") && *parent != "/workspace");
        if let Some(parent) = parent
            && !dirs.iter().any(|entry| entry == parent)
        {
            dirs.push(parent.to_string());
        }
    }
    for path in [plan.outputs_volume.as_str(), "/tmp"] {
        if !dirs.iter().any(|entry| entry == path) {
            dirs.push(path.to_string());
        }
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(output: &str) -> pc::SandboxSpec {
        pc::SandboxSpec {
            scope: "checkpoint-roots".into(),
            isolation: pc::IsolationClass::Container,
            mounts: Vec::new(),
            env: Vec::new(),
            packages: pc::PackageRequirements::default(),
            network: pc::NetworkPolicy::None,
            outputs_path: output.into(),
            requests: pc::ResourceRequests::default(),
            limits: pc::ResourceLimits::default(),
            filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
            lease_ttl_secs: Some(60),
            environment: None,
            command: Vec::new(),
            deny_tool_egress: false,
        }
    }

    #[test]
    fn retained_root_topology_shares_one_claim_but_never_a_subpath() {
        assert_eq!(
            writable_root_topology(true, 2),
            WritableRootTopology::Retained {
                claim_slot: 0,
                subpath_slot: 2,
            }
        );
        assert_eq!(
            writable_root_topology(false, 2),
            WritableRootTopology::Ephemeral { volume_slot: 2 }
        );
    }

    #[test]
    fn checkpoint_roots_are_canonical_and_persisted_evidence_fails_closed() {
        /* Checkpoint-root cause/effect table. C1=output is distinct/nested/tmp;
         * C2=persisted roots have the canonical order/shape; C3=root is blank,
         * relative, `/`, parent-traversing, reordered, or adds a fourth root.
         * R1 valid(C1)+C2 => one open-owned compact root set; R2 C3 => reject
         * before a checkpoint command. FMECA: open projection and a product
         * decorator restating roots can silently omit a new mount (S5/O3/D4=60).
         */
        let roots = checkpoint_writable_roots(&spec("/mnt/session/outputs")).unwrap();
        assert_eq!(roots, ["/workspace", "/mnt/session/outputs", "/tmp"], "R1");
        validate_checkpoint_writable_roots(&roots).unwrap();
        assert_eq!(
            checkpoint_writable_roots(&spec("/workspace/outputs")).unwrap(),
            ["/workspace", "/tmp"]
        );
        assert_eq!(
            checkpoint_writable_roots(&spec("/tmp")).unwrap(),
            ["/workspace", "/tmp"]
        );
        for invalid in ["", "/", "relative", "/workspace/../etc"] {
            assert!(
                checkpoint_writable_roots(&spec(invalid)).is_err(),
                "R2 {invalid}"
            );
        }
        for invalid in [
            vec!["/".into(), "/tmp".into()],
            vec!["/tmp".into(), "/workspace".into()],
            vec!["/workspace".into(), "../etc".into(), "/tmp".into()],
            vec![
                "/workspace".into(),
                "/outputs".into(),
                "/tmp".into(),
                "/var".into(),
            ],
        ] {
            assert!(
                validate_checkpoint_writable_roots(&invalid).is_err(),
                "R2 {invalid:?}"
            );
        }
    }
}
