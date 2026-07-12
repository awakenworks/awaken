//! Demand-driven execution shape (ADR-0107): decide *whether to realize a sandbox
//! at all* before paying its cost. A pure brain — no mounts, unrestricted egress,
//! `Workdir` isolation, no resource caps — needs no sandbox; anything that demands
//! isolation, controlled egress, mounts, or limits does. Pure and neutral so the
//! host can gate provisioning at the top of a turn.

use crate::sandbox::IsolationClass;
use crate::spec::SandboxSpec;

/// Whether a run needs a realized sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionShape {
    /// No isolation demand — run in-process; never provision a sandbox.
    NoSandbox,
    /// Real isolation/mount/egress/limit demand — provision a sandbox.
    Sandbox,
}

/// Classify a spec's execution shape. `Sandbox` iff it demands any of: a mount, a
/// restricted egress policy, stronger-than-`Workdir` isolation, or a resource cap.
/// Otherwise `NoSandbox` — the demand-driven gate that lets a pure/cloud brain skip
/// sandbox realization entirely.
#[must_use]
pub fn plan_shape(spec: &SandboxSpec) -> ExecutionShape {
    let demands_sandbox = !spec.mounts.is_empty()
        || spec.network.is_restricted()
        || spec.isolation > IsolationClass::Workdir
        || spec.limits.is_set();
    if demands_sandbox {
        ExecutionShape::Sandbox
    } else {
        ExecutionShape::NoSandbox
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vocab::{MountAccess, MountLifetime, MountRequirement, MountSource, NetworkPolicy};

    fn bare() -> SandboxSpec {
        SandboxSpec {
            scope: "t".into(),
            isolation: IsolationClass::Workdir,
            mounts: Vec::new(),
            env: Vec::new(),
            network: NetworkPolicy::Unrestricted,
            outputs_path: "/mnt/session/outputs".into(),
            limits: Default::default(),
            lease_ttl_secs: None,
            extra: None,
        }
    }

    fn a_mount() -> MountRequirement {
        MountRequirement {
            mount_id: "m".into(),
            source: MountSource::File {
                file_id: "f".into(),
                content_hash: None,
            },
            mount_path: "/workspace/x".into(),
            access: MountAccess::ReadOnly,
            lifetime: MountLifetime::PerRun,
            required: true,
        }
    }

    #[test]
    fn a_pure_brain_needs_no_sandbox() {
        assert_eq!(plan_shape(&bare()), ExecutionShape::NoSandbox);
    }

    #[test]
    fn a_mount_demands_a_sandbox() {
        let mut s = bare();
        s.mounts.push(a_mount());
        assert_eq!(plan_shape(&s), ExecutionShape::Sandbox);
    }

    #[test]
    fn restricted_egress_demands_a_sandbox() {
        let mut s = bare();
        s.network = NetworkPolicy::None;
        assert_eq!(plan_shape(&s), ExecutionShape::Sandbox);
        s.network = NetworkPolicy::Allowlist {
            hosts: vec!["api.anthropic.com".into()],
        };
        assert_eq!(plan_shape(&s), ExecutionShape::Sandbox);
    }

    #[test]
    fn stronger_isolation_demands_a_sandbox() {
        let mut s = bare();
        s.isolation = IsolationClass::Namespace;
        assert_eq!(plan_shape(&s), ExecutionShape::Sandbox);
    }

    #[test]
    fn a_resource_cap_demands_a_sandbox() {
        let mut s = bare();
        s.limits.memory_bytes = Some(128 * 1024 * 1024);
        assert_eq!(plan_shape(&s), ExecutionShape::Sandbox);
    }
}
