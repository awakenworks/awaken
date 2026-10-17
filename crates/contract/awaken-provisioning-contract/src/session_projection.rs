// @formal surface obligation=sandbox.admission_never_weakens_isolation
// @formal reason: Pure projections shared by Worker placement and Session realization: the isolation
// @formal reason: requirement derived from Session path causes and the Memory write-consistency decision.
//! Session path causes projected onto a sandbox spec. Worker placement and
//! Session realization share these pure projections, so a Worker never claims
//! requirements different from those it materializes.

use crate::{
    IsolationClass, MemoryWriteConsistency, MountAccess, SandboxControlServiceKind,
    SandboxIdleRetentionMode, SandboxIdleRetentionPolicy, SandboxRequirements, SandboxSpec,
};

/// Project every Session path cause once into the two neutral views consumed by
/// physical realization and Worker admission. [`SandboxRequirements::from_spec`]
/// remains the sole owner of concrete isolation/tool-transparency/path-fidelity
/// fields; this projection only supplies the complete cause and the matching
/// effective spec.
#[must_use]
pub fn session_sandbox_projection(
    spec: &SandboxSpec,
    has_repository: bool,
    opaque_process: bool,
) -> (SandboxSpec, SandboxRequirements) {
    let mut effective_spec = spec.clone();
    if has_repository {
        effective_spec
            .control_services
            .insert(SandboxControlServiceKind::RepositoryGitCredential);
    }
    let requires_path_fidelity =
        has_repository || opaque_process || spec.isolation >= IsolationClass::Namespace;
    let requirements = SandboxRequirements::from_spec(&effective_spec, requires_path_fidelity);
    // Copy the contract-owned isolation decision into the provider-visible
    // physical spec; do not reconstruct the Namespace upgrade elsewhere.
    effective_spec.isolation = requirements.isolation;
    (effective_spec, requirements)
}

/// The one pure Memory consistency decision shared by validation, effectful
/// staging, and final Sandbox assembly. Checkpoint-and-release may proceed only
/// when writable Memory is already write-through; read-only mounts and
/// Resident environments retain provider defaults.
#[must_use]
pub fn projected_memory_write_consistency(
    access: MountAccess,
    idle_retention: Option<&SandboxIdleRetentionPolicy>,
) -> MemoryWriteConsistency {
    if access == MountAccess::ReadWrite
        && idle_retention
            .is_some_and(|policy| policy.mode == SandboxIdleRetentionMode::CheckpointAndRelease)
    {
        MemoryWriteConsistency::WriteThroughRequired
    } else {
        MemoryWriteConsistency::ProviderDefault
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NetworkPolicy;

    fn workdir_spec() -> SandboxSpec {
        SandboxSpec {
            process_tree_supervision: false,
            scope: "thread-1".into(),
            isolation: IsolationClass::Workdir,
            environment: None,
            command: Vec::new(),
            deny_tool_egress: false,
            mounts: Vec::new(),
            env: Vec::new(),
            packages: Default::default(),
            network: NetworkPolicy::Unrestricted,
            outputs_path: "/mnt/session/outputs".into(),
            requests: Default::default(),
            limits: Default::default(),
            filesystem_continuity: crate::FilesystemContinuity::Retained,
            control_services: Default::default(),
            lease_ttl_secs: None,
        }
    }

    #[test]
    fn session_path_causes_select_one_spec_and_requirement_vector() {
        // Causes: C1 a Repository is projected; C2 the process is opaque;
        // C3 the spec already asks for Namespace isolation. Effects: E1 path
        // fidelity is required (Namespace isolation); E2 the Repository Git
        // credential service is added to the effective spec; E3 the effective
        // spec carries the contract's isolation decision.
        // | Rule | C1 | C2 | C3 | E1 | E2 |
        // | S1   | n  | n  | n  | n  | n  |
        // | S2   | y  | n  | n  | y  | y  |
        // | S3   | n  | y  | n  | y  | n  |
        // | S4   | n  | n  | y  | y  | n  |
        for (rule, repository, opaque, namespace, fidelity, git_service) in [
            ("S1", false, false, false, false, false),
            ("S2", true, false, false, true, true),
            ("S3", false, true, false, true, false),
            ("S4", false, false, true, true, false),
        ] {
            let mut spec = workdir_spec();
            if namespace {
                spec.isolation = IsolationClass::Namespace;
            }
            let (effective, required) = session_sandbox_projection(&spec, repository, opaque);
            assert_eq!(required.path_fidelity, fidelity, "{rule} E1");
            assert_eq!(
                effective
                    .control_services
                    .contains(&SandboxControlServiceKind::RepositoryGitCredential),
                git_service,
                "{rule} E2"
            );
            assert_eq!(effective.isolation, required.isolation, "{rule} E3");
        }
    }

    #[test]
    fn only_writable_memory_under_checkpoint_and_release_requires_write_through() {
        // Causes: C1 the mount is writable; C2 the Environment checkpoints and
        // releases when idle. Effect: write-through is required iff C1 and C2.
        let release = SandboxIdleRetentionPolicy {
            mode: SandboxIdleRetentionMode::CheckpointAndRelease,
            ..Default::default()
        };
        let resident = SandboxIdleRetentionPolicy::default();
        for (rule, access, policy, expected) in [
            (
                "M1",
                MountAccess::ReadWrite,
                Some(&release),
                MemoryWriteConsistency::WriteThroughRequired,
            ),
            (
                "M2",
                MountAccess::ReadOnly,
                Some(&release),
                MemoryWriteConsistency::ProviderDefault,
            ),
            (
                "M3",
                MountAccess::ReadWrite,
                Some(&resident),
                MemoryWriteConsistency::ProviderDefault,
            ),
            (
                "M4",
                MountAccess::ReadWrite,
                None,
                MemoryWriteConsistency::ProviderDefault,
            ),
        ] {
            assert_eq!(
                projected_memory_write_consistency(access, policy),
                expected,
                "{rule}"
            );
        }
    }
}
