// @formal surface requirement=resource.memory.memory_consistency
// @formal reason: The process composition selects one existing Memory mount adapter from the
// @formal reason: deployment tier and the original Resources source, bus and clock. Resource
// @formal reason: authorization, consistency and mount effects remain with their existing owners;
// @formal reason: this link is not a direct formal-proof claim.
//! Selection of the Memory mount data plane a co-located execution installs.

use std::sync::Arc;

use awaken_resource_contract::MemoryMountSource;
use awaken_session_executor::SessionExecutorBuilder;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlatformMemoryProjection {
    FusePreferred,
    CopyOnly,
}

fn platform_memory_projection(
    tier: awaken_deployment_config::SandboxTier,
) -> PlatformMemoryProjection {
    use awaken_deployment_config::SandboxTier;

    match tier {
        // Both run in a host-visible mount namespace. Namespace binds the host
        // projection into bwrap after realization, retaining live write-through.
        SandboxTier::Local | SandboxTier::Namespace => PlatformMemoryProjection::FusePreferred,
        // Docker and Podman cannot bind a host FUSE mount reliably. Kubernetes
        // owns native memory sidecars, but the same host adapter can still serve
        // housekeeping paths, so its portable fallback must remain bind-safe.
        SandboxTier::Docker | SandboxTier::Podman | SandboxTier::K8s => {
            PlatformMemoryProjection::CopyOnly
        }
    }
}

/// Select the governed Memory data plane's worker-side mount adapter
/// from the original Resources mount source and bus, unless the composition already
/// selected one (an explicitly injected Worker adapter is preserved). `tier`
/// is the Session sandbox tier the composition's Deployment selected.
/// Authorization has already selected workspace/store/access before this adapter
/// sees an opaque store id; no IAM vocabulary crosses this seam. The
/// co-located execution ([`crate::LocalExecution`]) and the scenario host
/// select it before building the Session executor.
#[must_use]
pub fn with_platform_memory_data_plane(
    dependencies: SessionExecutorBuilder,
    tier: awaken_deployment_config::SandboxTier,
    source: MemoryMountSource,
    invalidator: Arc<awaken_resource_contract::LocalInvalidator>,
) -> SessionExecutorBuilder {
    if dependencies.has_memory_mounter() {
        return dependencies;
    }
    let mounter = match platform_memory_projection(tier) {
        PlatformMemoryProjection::FusePreferred => {
            awaken_sandbox_memoryd::MemoryStoreMounter::fuse_preferred(
                source,
                invalidator,
                dependencies.clock(),
            )
            .expect("selected Memory source and root bus match")
        }
        PlatformMemoryProjection::CopyOnly => {
            awaken_sandbox_memoryd::MemoryStoreMounter::copy_only_from_source(source)
        }
    };
    dependencies.with_memory_mounter(Arc::new(mounter))
}

#[cfg(test)]
mod platform_memory_projection_tests {
    use super::{PlatformMemoryProjection, platform_memory_projection};
    use awaken_deployment_config::SandboxTier;

    /// Cause/effect graph: C1 host-visible projection namespace (Local/Namespace)
    /// produces E1 FUSE-preferred write-through; C2 OCI boundary (Docker/Podman)
    /// produces E2 copy-bind safety; C3 native-sidecar Kubernetes produces E3 a
    /// copy-safe host fallback while the runtime owns Session mounts.
    ///
    /// Decision table:
    /// | Rule | tier                 | host can expose FUSE | native sidecar | effect         |
    /// | P1   | Local, Namespace     | yes                  | no             | FusePreferred  |
    /// | P2   | Docker, Podman       | no                   | no             | CopyOnly       |
    /// | P3   | K8s                  | irrelevant           | yes            | CopyOnly host  |
    ///
    /// Enumerating every closed enum member also makes a newly added tier fail
    /// compilation until its projection contract is classified.
    #[test]
    fn sandbox_tier_selects_one_bind_compatible_memory_projection() {
        for tier in [SandboxTier::Local, SandboxTier::Namespace] {
            assert_eq!(
                platform_memory_projection(tier),
                PlatformMemoryProjection::FusePreferred
            );
        }
        for tier in [SandboxTier::Docker, SandboxTier::Podman, SandboxTier::K8s] {
            assert_eq!(
                platform_memory_projection(tier),
                PlatformMemoryProjection::CopyOnly
            );
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[tokio::test]
    async fn default_mount_degrades_to_copy_and_harvests_off_linux() {
        use awaken_provisioning_contract::{MemoryMounter as _, MountAccess, Realization};
        let bus = std::sync::Arc::new(awaken_resource_contract::LocalInvalidator::default());
        let resources =
            awaken_resource_persistence::ephemeral(awaken_time::system_clock(), bus.clone())
                .await
                .unwrap();
        let authorities = resources.authorities();
        authorities
            .memory_writer()
            .create("s", "/note.md", "v1")
            .await
            .unwrap();
        let source = authorities.memory_mount_source();
        let mounter = awaken_sandbox_memoryd::MemoryStoreMounter::fuse_preferred(
            source,
            bus,
            awaken_time::system_clock(),
        )
        .expect("same root bus");
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("platform-copy-fallback");
        let guard = match mounter.mount("s", &dir, MountAccess::ReadWrite).await {
            awaken_provisioning_contract::MemoryMountOutcome::Mounted(mount) => mount,
            _ => panic!("platform fallback must realize the real copy"),
        };
        assert_eq!(guard.realization(), Realization::Copy);
        assert_eq!(std::fs::read_to_string(dir.join("note.md")).unwrap(), "v1");
        std::fs::write(dir.join("note.md"), "v2").unwrap();
        guard.teardown().await.unwrap();
        assert_eq!(
            authorities
                .memory_queries()
                .get_by_path("s", "/note.md")
                .await
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("v2")
        );
    }
}
