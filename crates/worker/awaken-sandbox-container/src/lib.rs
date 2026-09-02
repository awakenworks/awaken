//! Container / Kubernetes sandbox provider (ADR-0041 Slice 5), below the neutral
//! seam. It realizes the same [`pc::SandboxProvider`]/[`pc::Sandbox`] ports as the
//! local tier, over a dependency-inverted [`ContainerRuntime`] port so the provider
//! *logic* is exercised by a fake while the real bollard (Docker), kube (K8s), and
//! rootless-podman CLI clients slot in as adapters behind the port (kept out of the
//! neutral contract; never depended on by the agents plane — G2).
//!
//! Three decisions from the ADR amendment are made concrete and unit-testable here
//! as **pure planners** (no daemon needed):
//! - **Session-owned environment**: one durable container holds the workspace and
//!   every Native/ACP attempt is an exec process inside it;
//! - **native GC**: the Pod carries an `owner_uid` (an `ownerReference`) so an
//!   orphaned sandbox is reaped by the platform, not a bespoke reaper;
//! - **out-of-band artifacts**: outputs live on a volume ([`ContainerPlan::outputs_volume`]),
//!   retrieved without streaming through the control plane.

use async_trait::async_trait;
use awaken_agent_channel::AgentChannel;
use awaken_provisioning_contract as pc;
use awaken_resource_contract::content_id as content_fingerprint;
pub use awaken_sandbox_control::{
    PublishedSandboxControlService, SandboxControlPublishError, SandboxControlService,
    SandboxControlServiceKind, SandboxControlServicePublisher,
};
use std::sync::Arc;

mod cache_volume;
mod control;
use control::ContainerControlPublicationRegistry;
mod cgroup;
mod egress;
mod files;
mod live_inputs;
mod packages;
mod podman_plan;
mod process_env;
mod provider_contract;
mod provider_realization;
use process_env::environment_keepalive_command;
pub use process_env::runtime_configuration_homes;
mod recovery;
mod resident_hand;
mod restore_target;
mod runtime;
mod secret;
mod writable;
pub use cgroup::CgroupCaps;
pub use egress::{
    AllowlistCapability, AllowlistProxy, EgressError, EgressRealization, EgressRealizationIdentity,
    ForwardProxy, NetworkMode, egress_plan, egress_plan_with_allowlist, normalize_hostname,
};
pub use live_inputs::{LIVE_INPUTS_ROOT, live_input_relative_path};
pub use packages::package_containerfile;
pub use podman_plan::{RootfsError, RootfsPlan, podman_run_argv, rootfs_plan};
use podman_plan::{image_of, rootfs_of};
pub use provider_contract::{
    ContainerCreateAttempt, ContainerEffectFence, ContainerEffectFenceSource, ContainerEnvironment,
    ContainerEnvironmentAdoption, ContainerEnvironmentProvider, ContainerObservationExpectation,
    ContainerRealizationContext, ContainerRealizationIntent, ContainerRealizationNamespace,
    EnvironmentFile,
};
pub use resident_hand::ResidentHandConfig;
#[cfg(any(test, feature = "docker", feature = "podman"))]
use restore_target::remove_host_staging_path;
pub(crate) use restore_target::restoration_plan_fingerprint;
#[cfg(any(feature = "docker", feature = "podman"))]
pub(crate) use restore_target::restore_container_name;
#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
pub(crate) use restore_target::{
    RESTORE_PLAN_LABEL, restoration_evidence_from_metadata, restoration_metadata,
};
use restore_target::{restoration_runtime_scope, retained_host_staging};
pub use runtime::{
    ContainerRuntime, ContainerState, K8sContinuationVolume, MemoryMount, PackageImageProvisioner,
    RuntimeAgentProcess, RuntimeError, RuntimeRestoreTarget, SandboxControlBindingRequest,
};
pub(crate) use runtime::{MANAGED_SANDBOX_LABEL, runtime_owner_id};
#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
pub(crate) use runtime::{
    RUNTIME_OWNER_LABEL, SANDBOX_ATTEMPT_LABEL, container_effect_fence_from_values,
    sandbox_scope_identity,
};
#[cfg(any(feature = "docker", feature = "podman"))]
pub(crate) use runtime::{
    SANDBOX_ADOPTION_LABEL, SANDBOX_EFFECT_EPOCH_LABEL, SANDBOX_EFFECT_EXPIRY_LABEL,
    SANDBOX_EFFECT_LABEL, SANDBOX_EFFECT_OWNER_LABEL, SANDBOX_EFFECT_RUNTIME_LABEL,
    SANDBOX_REALIZATION_LABEL, SANDBOX_SCOPE_LABEL, container_effect_label_values,
    runtime_container_name,
};
use runtime::{
    allowlist_capability_advertised, container_adoption_fingerprint, container_capabilities,
    container_realization_fingerprint, container_runtime_unix_now_ms,
};
pub use secret::SecretBytes;
pub use writable::{
    checkpoint_writable_roots, checkpoint_writable_roots_from_output_path,
    validate_checkpoint_writable_roots, writable_dirs,
};

// ── Pure planners ─────────────────────────────────────────────────────────────

/// One realized bind inside the container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindPlan {
    pub source_ref: String,
    pub mount_path: String,
    pub read_only: bool,
    /// Self-contained typed `Inline` content carried in the plan itself,
    /// not a host ref — the tier realizes it in-band without a blob store: docker/podman
    /// stage it to a host file and repoint `source_ref`; k8s projects it as a ConfigMap
    /// volume. `None` for ref-backed binds (File/Resource store id, CacheVolume path).
    pub content: Option<String>,
    /// Self-contained BINARY single-file content — a resolved File/Resource whose bytes are
    /// not valid UTF-8. docker/podman bind the staged host file (byte-safe); k8s projects it
    /// as a ConfigMap `binaryData` entry (its text `data` counterpart is `content`).
    pub content_bytes: Option<Vec<u8>>,
    /// Broker-materialized secret bytes for a remote runtime. Debug is redacted so
    /// logging a ContainerPlan cannot expose an OAuth credential.
    pub secret_content: Option<SecretBytes>,
    /// A durable writable Secret whose final bytes must be committed to its broker.
    pub secret_writeback: bool,
    /// Exact credential file path inside a directory bind. Docker/Podman mount the
    /// writable config directory; Kubernetes reads this file via exec for writeback.
    pub credential_file_path: Option<String>,
}

/// A neutral container plan rendered from a [`pc::SandboxSpec`] — the input a
/// bollard `create_container` (or the k8s planner) consumes. Pure and testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerPlan {
    pub image: String,
    /// The Session environment's PID-1 command. Attempts execute separately through
    /// [`ContainerRuntime::spawn`] / [`ContainerRuntime::spawn_agent`].
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    pub control_services: std::collections::BTreeSet<SandboxControlServiceKind>,
    pub packages: pc::PackageRequirements,
    pub binds: Vec<BindPlan>,
    /// Out-of-band outputs volume mount path (artifacts leave via the volume).
    pub outputs_volume: String,
    pub network: NetworkMode,
    pub egress_identity: EgressRealizationIdentity,
    pub requests: pc::ResourceRequests,
    pub limits: pc::ResourceLimits,
    /// Exact writable-filesystem lifecycle requested by the neutral spec.
    pub filesystem_continuity: pc::FilesystemContinuity,
    /// Memory-store mounts, realized as runtime-native writable volumes (NOT binds).
    pub memory_mounts: Vec<MemoryMount>,
    /// The rootfs the agent runs on (Image / private IsolatedRoot). Honored by the
    /// rootless-podman adapter; the docker/k8s adapters run `image` directly.
    pub rootfs: RootfsPlan,
}

#[cfg(test)]
mod planner_tests {
    use super::*;

    #[test]
    fn rootfs_plan_maps_image_and_isolated_roots() {
        assert_eq!(
            rootfs_plan(&pc::EnvironmentKind::Image {
                reference: "ghcr.io/x:1".into()
            })
            .unwrap(),
            RootfsPlan::Image("ghcr.io/x:1".into())
        );
        assert_eq!(
            rootfs_plan(&pc::EnvironmentKind::Sandbox).unwrap(),
            RootfsPlan::HostUserland
        );
        assert_eq!(
            rootfs_plan(&pc::EnvironmentKind::IsolatedRoot {
                base: pc::RootfsSource::Dir {
                    path_template: "/roots/{scope}".into()
                },
                writable_base: true,
            })
            .unwrap(),
            RootfsPlan::RootDir {
                path_template: "/roots/{scope}".into(),
                writable: true,
            }
        );
        assert_eq!(
            rootfs_plan(&pc::EnvironmentKind::IsolatedRoot {
                base: pc::RootfsSource::Tarball {
                    reference: "blob://root".into()
                },
                writable_base: false,
            })
            .unwrap(),
            RootfsPlan::RootTarball {
                reference: "blob://root".into(),
                writable: false,
            }
        );
    }

    #[test]
    fn rootfs_plan_rejects_non_container_tiers() {
        assert_eq!(
            rootfs_plan(&pc::EnvironmentKind::Scope),
            Err(RootfsError::NotAContainerRootfs)
        );
        assert_eq!(
            rootfs_plan(&pc::EnvironmentKind::LocalDir {
                path_template: "/tmp/{scope}".into()
            }),
            Err(RootfsError::NotAContainerRootfs)
        );
    }

    fn podman_plan() -> ContainerPlan {
        ContainerPlan {
            image: "ghcr.io/awaken/sandbox:1".into(),
            command: vec!["claude".into(), "--acp".into()],
            env: vec![("TZ".into(), "UTC".into())],
            control_services: Default::default(),
            packages: Default::default(),
            binds: vec![BindPlan {
                source_ref: "/host/data".into(),
                mount_path: "/data".into(),
                read_only: true,
                content: None,
                content_bytes: None,
                secret_content: None,
                secret_writeback: false,
                credential_file_path: None,
            }],
            outputs_volume: "/mnt/session/outputs".into(),
            network: NetworkMode::None,
            egress_identity: EgressRealizationIdentity {
                network: NetworkMode::None,
                proxy_endpoint: None,
                capability_ttl_secs: None,
                issuer_revision: None,
                ephemeral_capability: false,
            },
            requests: pc::ResourceRequests::default(),
            limits: pc::ResourceLimits {
                cpu_millis: Some(1500),
                memory_bytes: Some(1 << 30),
                pids: Some(256),
                disk_bytes: None,
            },
            filesystem_continuity: pc::FilesystemContinuity::Retained,
            memory_mounts: Vec::new(),
            rootfs: RootfsPlan::HostUserland,
        }
    }

    // Find the value that follows `flag` in the argv (for order-independent asserts).
    fn arg_after<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
        argv.iter()
            .position(|a| a == flag)
            .and_then(|i| argv.get(i + 1))
            .map(String::as_str)
    }

    #[test]
    fn podman_run_argv_hardens_with_readonly_rootfs_and_writable_tmpfs() {
        let argv = podman_run_argv("r", &podman_plan(), &RootfsPlan::HostUserland);
        assert!(
            argv.iter().any(|a| a == "--read-only"),
            "rootfs is read-only"
        );
        // outputs + /tmp are the writable set (as tmpfs); declared binds stay writable.
        let tmpfs: Vec<&str> = argv
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "--tmpfs")
            .filter_map(|(i, _)| argv.get(i + 1).map(String::as_str))
            .collect();
        assert!(tmpfs.iter().any(|value| value.starts_with("/workspace:")));
        assert!(
            tmpfs
                .iter()
                .any(|value| value.starts_with("/mnt/session/outputs:"))
        );
        assert!(tmpfs.iter().any(|value| value.starts_with("/tmp:")));
        assert!(tmpfs.iter().all(|value| value.contains("mode=1777")));
    }

    #[test]
    fn writable_dirs_are_workspace_outputs_and_tmp_deduped() {
        let mut plan = podman_plan();
        assert_eq!(
            writable_dirs(&plan),
            vec!["/workspace", "/mnt/session/outputs", "/tmp"]
        );
        plan.outputs_volume = "/tmp".into();
        assert_eq!(writable_dirs(&plan), vec!["/workspace", "/tmp"]);
        plan.outputs_volume = "/workspace".into();
        assert_eq!(writable_dirs(&plan), vec!["/workspace", "/tmp"]);
    }

    #[test]
    fn writable_dirs_include_inline_workspace_file_parents_once() {
        let mut plan = podman_plan();
        plan.binds.extend([
            BindPlan {
                source_ref: "file-a".into(),
                mount_path: "/workspace/.mnt/workspace/a.txt".into(),
                read_only: true,
                content: Some("a".into()),
                content_bytes: None,
                secret_content: None,
                secret_writeback: false,
                credential_file_path: None,
            },
            BindPlan {
                source_ref: "file-b".into(),
                mount_path: "/workspace/.mnt/workspace/b.txt".into(),
                read_only: true,
                content: None,
                content_bytes: Some(vec![0xff]),
                secret_content: None,
                secret_writeback: false,
                credential_file_path: None,
            },
        ]);
        assert_eq!(
            writable_dirs(&plan),
            vec![
                "/workspace",
                "/workspace/.mnt/workspace",
                "/mnt/session/outputs",
                "/tmp",
            ]
        );
    }

    #[test]
    fn podman_run_argv_maps_init_network_limits_env_binds_and_command() {
        let argv = podman_run_argv("run-1", &podman_plan(), &RootfsPlan::Image("img:2".into()));
        assert!(argv.starts_with(&["run".into(), "-d".into(), "--init".into()]));
        assert_eq!(arg_after(&argv, "--name"), Some("run-1"));
        assert_eq!(arg_after(&argv, "--entrypoint"), Some(""));
        // network None → --network none
        assert_eq!(arg_after(&argv, "--network"), Some("none"));
        // cgroup: swap pinned to memory, cpus decimal, pids
        assert_eq!(arg_after(&argv, "--memory"), Some("1073741824"));
        assert_eq!(arg_after(&argv, "--memory-swap"), Some("1073741824"));
        assert_eq!(arg_after(&argv, "--cpus"), Some("1.500"));
        assert_eq!(arg_after(&argv, "--pids-limit"), Some("256"));
        // env + read-only bind
        assert_eq!(arg_after(&argv, "-e"), Some("TZ=UTC"));
        assert_eq!(arg_after(&argv, "-v"), Some("/host/data:/data:ro"));
        // Session environment: the image then the PID-1 keepalive are the tail.
        assert_eq!(&argv[argv.len() - 3..], &["img:2", "claude", "--acp"]);
    }

    #[test]
    fn podman_run_argv_uses_rootfs_overlay_for_a_read_only_private_root() {
        let mut plan = podman_plan();
        plan.network = NetworkMode::Open; // no --network flag
        let ro = podman_run_argv(
            "r",
            &plan,
            &RootfsPlan::RootDir {
                path_template: "/roots/base".into(),
                writable: false,
            },
        );
        // read-only base → `--rootfs <dir>:O` (overlay keeps the base untouched)
        assert_eq!(arg_after(&ro, "--rootfs"), Some("/roots/base:O"));
        assert!(
            !ro.iter().any(|a| a == "--network"),
            "Open egress adds no flag"
        );

        let rw = podman_run_argv(
            "r",
            &plan,
            &RootfsPlan::RootDir {
                path_template: "/roots/base".into(),
                writable: true,
            },
        );
        assert_eq!(arg_after(&rw, "--rootfs"), Some("/roots/base"));
    }

    #[test]
    fn podman_run_argv_host_userland_runs_the_default_image() {
        let argv = podman_run_argv("r", &podman_plan(), &RootfsPlan::HostUserland);
        // HostUserland → the plan's image is the run target (no --rootfs).
        assert!(!argv.iter().any(|a| a == "--rootfs"));
        assert!(argv.contains(&"ghcr.io/awaken/sandbox:1".to_string()));
    }

    #[test]
    fn podman_run_argv_maps_a_disk_cap_to_storage_opt() {
        // A declared disk cap must reach the writable-layer limit (`--storage-opt
        // size=`), otherwise the advertised resource limit is never enforced.
        let mut plan = podman_plan();
        plan.limits.disk_bytes = Some(1 << 30);
        let argv = podman_run_argv("r", &plan, &RootfsPlan::HostUserland);
        assert_eq!(arg_after(&argv, "--storage-opt"), Some("size=1073741824"));
    }
}

/// A host staging directory holding the bytes of inline mounts (codex `config.toml`,
/// ADR-0038 resource content) materialized for a container's lifetime, then removed when
/// the sandbox is dropped (after the container is gone). The container tier binds host
/// paths, so self-contained content (`Inline` / `Other{content}`) is written here and the
/// bind repointed at the host file — the counterpart of the bwrap tier's `resolve_source`
/// plus write. Content-addressed `File`/`Resource` (a store id, not bytes) still needs a
/// blob-store resolve on this tier; the ACP resource path rides `Other{content}` so it
/// works today.
#[derive(Debug)]
pub(crate) struct StagingGuard {
    path: std::path::PathBuf,
    remove_on_drop: bool,
}

impl StagingGuard {
    fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn retain_for_restoration(&mut self) {
        self.remove_on_drop = false;
    }

    fn retained(path: std::path::PathBuf) -> Self {
        Self {
            path,
            remove_on_drop: false,
        }
    }

    fn remove(mut self) {
        self.remove_on_drop = true;
    }
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[derive(Debug)]
struct SecretWriteback {
    reference: String,
    /// Host-bind runtimes harvest from the live container first and may fall
    /// back to their exact staged file. Native runtimes never create that
    /// redundant plaintext copy and therefore have no fallback path.
    staged_path: Option<std::path::PathBuf>,
    mount_path: String,
}

/// One frozen Memory projection reused by first materialization and cold
/// terminal reconciliation. It contains only Resource-owned coordinates from
/// the SandboxSpec; runtime bytes and mount handles remain effect-scoped.
#[derive(Clone, Debug, PartialEq, Eq)]
struct MemoryProjection {
    store_id: String,
    store_reference: String,
    mount_path: String,
    access: pc::MountAccess,
    write_consistency: pc::MemoryWriteConsistency,
}

fn memory_projections(spec: &pc::SandboxSpec) -> Vec<MemoryProjection> {
    spec.mounts
        .iter()
        .filter_map(|mount| match &mount.source {
            pc::MountSource::MemoryStore {
                store_id,
                materialization_reference,
                write_consistency,
            } => Some(MemoryProjection {
                store_id: store_id.clone(),
                store_reference: materialization_reference
                    .as_deref()
                    .unwrap_or(store_id)
                    .to_owned(),
                mount_path: mount.mount_path.clone(),
                access: mount.access,
                write_consistency: *write_consistency,
            }),
            _ => None,
        })
        .collect()
}

fn validated_recovered_memory_projections(
    spec: &pc::SandboxSpec,
    handle: &pc::SandboxHandle,
) -> Result<Vec<MemoryProjection>, pc::SandboxError> {
    let projections = memory_projections(spec);
    let materializations = handle.memory_materializations()?;
    if projections.is_empty() {
        if materializations.is_some_and(|materializations| !materializations.is_empty()) {
            return Err(pc::SandboxError::new(
                "container handle carries Memory evidence absent from the frozen specification",
            ));
        }
        return Ok(Vec::new());
    }
    let materializations = materializations.ok_or_else(|| {
        pc::SandboxError::new(
            "legacy container handle cannot reconstruct copy-backed Memory participants",
        )
    })?;
    if materializations.len() != projections.len() {
        return Err(pc::SandboxError::new(
            "container Memory evidence does not exactly cover the frozen specification",
        ));
    }
    projections
        .into_iter()
        .map(|projection| {
            let materialization = materializations
                .iter()
                .find(|materialization| {
                    materialization.store_id == projection.store_id
                        && materialization.mount_path == projection.mount_path
                })
                .ok_or_else(|| {
                    pc::SandboxError::new(
                        "container Memory evidence differs from the frozen specification",
                    )
                })?;
            materialization.validate()?;
            Ok(projection)
        })
        .collect()
}

fn secret_writeback_projection(mount: &pc::MountRequirement) -> Option<SecretWriteback> {
    if !mount.is_secret_writeback() {
        return None;
    }
    let pc::MountSource::Secret { reference, .. } = &mount.source else {
        return None;
    };
    Some(SecretWriteback {
        reference: reference.clone(),
        staged_path: None,
        mount_path: mount.mount_path.clone(),
    })
}

#[derive(Default)]
struct StagedMounts {
    guard: Option<StagingGuard>,
    secret_writebacks: Vec<SecretWriteback>,
    memory: Vec<StagedMemoryMount>,
}

struct StagedMemoryMount {
    handle: Box<dyn pc::MemoryMount>,
    host_path: std::path::PathBuf,
    mount_path: String,
    access: pc::MountAccess,
    materialization: Option<pc::MemoryMaterializationEvidence>,
    native_runtime_volume: bool,
    writeback_prepared: bool,
}

impl StagedMounts {
    /// Tear down every effect-scoped Memory participant after the runtime has
    /// proved that no backend object was committed. A single failed teardown
    /// must not prevent the remaining independent handles from being tried.
    async fn teardown_memory_after_definite_no_backend_effect(
        &self,
    ) -> Result<(), pc::SandboxError> {
        let mut first_error = None;
        for mount in &self.memory {
            if let Err(error) = mount.handle.teardown().await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Preserve process-local participant ownership after an ambiguous backend
    /// mutation or a failed explicit teardown. Docker/Podman host binds and
    /// Memory mount handles cannot be reconstructed by a later process, so
    /// dropping either here could break a physical object that actually
    /// committed or erase the only retryable cleanup handle. This intentionally
    /// trades liveness/capacity for safety; the shared CurrentAttemptOnly kernel
    /// keeps later processes fail-closed instead of claiming recovery.
    fn retain_participants_without_cleanup(self) {
        std::mem::forget(self);
    }
}

impl std::fmt::Debug for StagedMounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StagedMounts")
            .field("guard", &self.guard)
            .field("secret_writebacks", &self.secret_writebacks)
            .field("memory_mounts", &self.memory.len())
            .finish()
    }
}

async fn reject_unaccepted_memory_mount(
    handle: Box<dyn pc::MemoryMount>,
    cause: pc::SandboxError,
) -> pc::SandboxError {
    match handle.teardown().await {
        Ok(()) => cause,
        Err(cleanup_error) => {
            // `MemoryMount::teardown` is retryable by contract. If it fails,
            // dropping the only handle would silently convert a cleanup error
            // into leaked/unknown physical state; retain it for process-lifetime
            // safety just like an ambiguous backend participant.
            std::mem::forget(handle);
            pc::SandboxError::new(format!(
                "{cause}; Memory participant cleanup failed: {cleanup_error}"
            ))
        }
    }
}

async fn stage_memory_binds(
    spec: &pc::SandboxSpec,
    plan: &mut ContainerPlan,
    staged: &mut StagedMounts,
    mounter: Option<Arc<dyn pc::MemoryMounter>>,
    native_runtime_volume: bool,
) -> Result<(), pc::SandboxError> {
    let memory = memory_projections(spec);
    if memory.is_empty() {
        return Ok(());
    }
    let mounter = mounter
        .ok_or_else(|| pc::SandboxError::new("container MemoryStore mount has no MemoryMounter"))?;
    let root = staging_dir(&mut staged.guard, &spec.scope)?;
    for projection in memory {
        let host_path = root.join(format!("memory-{}", stage_name(&projection.mount_path)));
        let handle = mounter
            .mount(&projection.store_reference, &host_path, projection.access)
            .await?;
        let realization = handle.realization();
        let materialization = match realization {
            pc::Realization::Copy => {
                let Some(heads) = handle.materialization_heads() else {
                    return Err(reject_unaccepted_memory_mount(
                        handle,
                        pc::SandboxError::new(
                            "copy-backed Memory mount omitted its original durable heads",
                        ),
                    )
                    .await);
                };
                let evidence = pc::MemoryMaterializationEvidence::new(
                    projection.store_id.clone(),
                    projection.mount_path.clone(),
                    heads,
                );
                match evidence {
                    Ok(evidence) => Some(evidence),
                    Err(error) => {
                        return Err(reject_unaccepted_memory_mount(handle, error).await);
                    }
                }
            }
            pc::Realization::Fuse => None,
            _ => {
                return Err(reject_unaccepted_memory_mount(
                    handle,
                    pc::SandboxError::new("MemoryStore mount returned an unsupported realization"),
                )
                .await);
            }
        };
        if projection.write_consistency == pc::MemoryWriteConsistency::WriteThroughRequired
            && realization != pc::Realization::Fuse
        {
            return Err(reject_unaccepted_memory_mount(
                handle,
                pc::SandboxError::new("MemoryStore mount requires write-through FUSE realization"),
            )
            .await);
        }
        if native_runtime_volume && realization != pc::Realization::Copy {
            return Err(reject_unaccepted_memory_mount(
                handle,
                pc::SandboxError::new(
                    "remote container Memory volume requires a copy-capable MemoryMounter",
                ),
            )
            .await);
        }
        #[cfg(unix)]
        if let Err(error) = make_memory_tree_accessible(&host_path, projection.access) {
            return Err(reject_unaccepted_memory_mount(handle, error).await);
        }
        if native_runtime_volume {
            let snapshot_tar = match memory_snapshot_tar(&host_path) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    return Err(reject_unaccepted_memory_mount(handle, error).await);
                }
            };
            let planned = plan
                .memory_mounts
                .iter_mut()
                .find(|planned| planned.mount_path == projection.mount_path)
                .ok_or_else(|| pc::SandboxError::new("Memory mount disappeared from plan"));
            let planned = match planned {
                Ok(planned) => planned,
                Err(error) => {
                    return Err(reject_unaccepted_memory_mount(handle, error).await);
                }
            };
            planned.snapshot_tar = snapshot_tar;
            planned.access = projection.access;
        } else {
            plan.binds.push(BindPlan {
                source_ref: host_path.to_string_lossy().into_owned(),
                mount_path: projection.mount_path.clone(),
                read_only: projection.access == pc::MountAccess::ReadOnly,
                content: None,
                content_bytes: None,
                secret_content: None,
                secret_writeback: false,
                credential_file_path: None,
            });
        }
        staged.memory.push(StagedMemoryMount {
            handle,
            host_path,
            mount_path: projection.mount_path,
            access: projection.access,
            materialization,
            native_runtime_volume,
            writeback_prepared: false,
        });
    }
    Ok(())
}

const MAX_MEMORY_SNAPSHOT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_MEMORY_SNAPSHOT_FILES: usize = 10_000;

fn memory_snapshot_tar(root: &std::path::Path) -> Result<Vec<u8>, pc::SandboxError> {
    fn append(
        archive: &mut tar::Builder<Vec<u8>>,
        root: &std::path::Path,
        directory: &std::path::Path,
        files: &mut usize,
        bytes: &mut u64,
    ) -> Result<(), pc::SandboxError> {
        let entries = std::fs::read_dir(directory)
            .map_err(|error| pc::SandboxError::new(error.to_string()))?;
        for entry in entries {
            let entry = entry.map_err(|error| pc::SandboxError::new(error.to_string()))?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|error| pc::SandboxError::new(error.to_string()))?;
            if metadata.file_type().is_symlink() {
                return Err(pc::SandboxError::new(
                    "Memory snapshot must not contain symbolic links",
                ));
            }
            if metadata.is_dir() {
                append(archive, root, &path, files, bytes)?;
                continue;
            }
            if !metadata.is_file() {
                return Err(pc::SandboxError::new(
                    "Memory snapshot contains a non-regular file",
                ));
            }
            *files = files.saturating_add(1);
            *bytes = bytes.saturating_add(metadata.len());
            if *files > MAX_MEMORY_SNAPSHOT_FILES || *bytes > MAX_MEMORY_SNAPSHOT_BYTES {
                return Err(pc::SandboxError::new(
                    "Memory snapshot exceeds the remote-container projection limit",
                ));
            }
            let relative = path
                .strip_prefix(root)
                .map_err(|error| pc::SandboxError::new(error.to_string()))?;
            archive
                .append_path_with_name(&path, relative)
                .map_err(|error| pc::SandboxError::new(error.to_string()))?;
        }
        Ok(())
    }

    let mut archive = tar::Builder::new(Vec::new());
    let mut files = 0;
    let mut bytes = 0;
    append(&mut archive, root, root, &mut files, &mut bytes)?;
    archive
        .into_inner()
        .map_err(|error| pc::SandboxError::new(error.to_string()))
}

fn replace_memory_snapshot(
    root: &std::path::Path,
    files: &[EnvironmentFile],
) -> Result<(), pc::SandboxError> {
    if root.exists() {
        std::fs::remove_dir_all(root).map_err(|error| pc::SandboxError::new(error.to_string()))?;
    }
    std::fs::create_dir_all(root).map_err(|error| pc::SandboxError::new(error.to_string()))?;
    let mut total = 0_u64;
    for file in files {
        let relative = std::path::Path::new(&file.path);
        if relative.is_absolute()
            || file.path.is_empty()
            || relative
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(pc::SandboxError::new(
                "unsafe path in harvested Memory snapshot",
            ));
        }
        total = total.saturating_add(file.bytes.len() as u64);
        if total > MAX_MEMORY_SNAPSHOT_BYTES || files.len() > MAX_MEMORY_SNAPSHOT_FILES {
            return Err(pc::SandboxError::new(
                "harvested Memory snapshot exceeds the projection limit",
            ));
        }
        let destination = root.join(relative);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| pc::SandboxError::new(error.to_string()))?;
        }
        std::fs::write(destination, &file.bytes)
            .map_err(|error| pc::SandboxError::new(error.to_string()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn make_memory_tree_accessible(
    root: &std::path::Path,
    access: pc::MountAccess,
) -> Result<(), pc::SandboxError> {
    use std::os::unix::fs::PermissionsExt;
    for entry in std::fs::read_dir(root).map_err(|e| pc::SandboxError::new(e.to_string()))? {
        let path = entry
            .map_err(|e| pc::SandboxError::new(e.to_string()))?
            .path();
        let metadata =
            std::fs::symlink_metadata(&path).map_err(|e| pc::SandboxError::new(e.to_string()))?;
        if metadata.is_dir() {
            make_memory_tree_accessible(&path, access)?;
        }
        let mode = if metadata.is_dir() {
            if access == pc::MountAccess::ReadWrite {
                0o777
            } else {
                0o555
            }
        } else if access == pc::MountAccess::ReadWrite {
            0o666
        } else {
            0o444
        };
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| pc::SandboxError::new(e.to_string()))?;
    }
    let mode = if access == pc::MountAccess::ReadWrite {
        0o777
    } else {
        0o555
    };
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(mode))
        .map_err(|e| pc::SandboxError::new(e.to_string()))
}

/// The per-run host staging dir (created once, lazily), kept alive by the returned guard.
pub(crate) fn staging_dir(
    guard: &mut Option<StagingGuard>,
    scope: &str,
) -> Result<std::path::PathBuf, pc::SandboxError> {
    if let Some(g) = guard {
        return Ok(g.path().to_path_buf());
    }
    // `create_dir` is atomic. The runtime-instance id plus a bounded collision suffix
    // avoids predictable-path/symlink attacks without adding a filesystem utility
    // dependency to this low-level crate.
    let prefix = format!(
        "awaken-acp-stage-{}-{}",
        stage_name(scope),
        runtime_owner_id()
    );
    let mut created = None;
    for attempt in 0..16 {
        let candidate = std::env::temp_dir().join(format!("{prefix}-{attempt}"));
        #[cfg(unix)]
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(not(unix))]
        let builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&candidate) {
            Ok(()) => {
                created = Some(candidate);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(err(RuntimeError::Backend(format!("stage dir: {error}"))));
            }
        }
    }
    let d = created.ok_or_else(|| {
        err(RuntimeError::Backend(
            "could not allocate a unique staging directory".into(),
        ))
    })?;
    let path = d.clone();
    *guard = Some(StagingGuard {
        path: d,
        remove_on_drop: true,
    });
    Ok(path)
}

/// Flatten a sandbox-absolute mount path to a single staging filename.
fn stage_name(mount_path: &str) -> String {
    mount_path
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The declared content hash of a mount source, if any (verified fail-closed).
fn declared_hash(source: &pc::MountSource) -> Option<&str> {
    match source {
        pc::MountSource::File { content_hash, .. }
        | pc::MountSource::Resource { content_hash, .. }
        | pc::MountSource::Secret { content_hash, .. }
        | pc::MountSource::InlineBytes { content_hash, .. } => content_hash.as_deref(),
        _ => None,
    }
}

/// Verify resolved bytes against a declared hash; fail closed on mismatch.
fn verify_hash(source: &pc::MountSource, bytes: &[u8]) -> Result<(), pc::SandboxError> {
    if let Some(expected) = declared_hash(source) {
        let got = content_fingerprint(bytes);
        if got != expected {
            return Err(err(RuntimeError::Backend(format!(
                "mount content hash mismatch: declared {expected}, realized {got}"
            ))));
        }
    }
    Ok(())
}

/// Resolve a by-reference mount's bytes: the in-memory seed first, then the injected
/// content-addressed store (A-G17 — the provider links no durable store). `None` for a
/// source that carries no store id (Inline/Other/CacheVolume/MemoryStore) or an id absent
/// from both; the caller fails a *required* miss closed.
async fn resolve_blob(
    source: &pc::MountSource,
    seed: &std::collections::HashMap<String, Vec<u8>>,
    store: &Option<Arc<dyn pc::BlobSource>>,
    secret_broker: &Option<Arc<dyn pc::SecretBroker>>,
) -> Result<Option<Vec<u8>>, pc::SandboxError> {
    let id = match source {
        pc::MountSource::File { file_id, .. } => file_id.as_str(),
        pc::MountSource::Resource { resource_id, .. } => resource_id.as_str(),
        pc::MountSource::Secret { reference, .. } => {
            if let Some(broker) = secret_broker {
                return broker.materialize(reference).await.map(Some);
            }
            reference.as_str()
        }
        _ => return Ok(None),
    };
    if let Some(bytes) = seed.get(id) {
        return Ok(Some(bytes.clone()));
    }
    if let Some(store) = store
        && let Some(bytes) = store.get(id).await
    {
        return Ok(Some(bytes));
    }
    Ok(None)
}

/// Resolve + materialize every mount's bytes for the container. Self-contained
/// content ships as-is; reference sources resolve through [`resolve_blob`].
/// Docker/Podman stage one host bind, while native Kubernetes projects the
/// resolved bytes directly from the plan. Required misses fail closed.
async fn resolve_and_stage(
    spec: &pc::SandboxSpec,
    binds: &mut [BindPlan],
    seed: &std::collections::HashMap<String, Vec<u8>>,
    store: &Option<Arc<dyn pc::BlobSource>>,
    secret_broker: &Option<Arc<dyn pc::SecretBroker>>,
    persistent_volume_claims: bool,
    host_bind_materialization: bool,
) -> Result<StagedMounts, pc::SandboxError> {
    use std::collections::HashMap;
    let by_path: HashMap<&str, &pc::MountRequirement> = spec
        .mounts
        .iter()
        .map(|m| (m.mount_path.as_str(), m))
        .collect();
    let mut guard: Option<StagingGuard> = None;
    let mut secret_writebacks = Vec::new();
    for bind in binds.iter_mut() {
        // 1. Obtain the bytes this bind realizes, or skip a bind that needs no staging.
        let had_content = bind.content.is_some() || bind.content_bytes.is_some();
        let bytes: Vec<u8> = if let Some(contents) = &bind.content {
            // Inline / Other{content} — self-contained, captured by `binds_of`.
            contents.clone().into_bytes()
        } else if let Some(contents) = &bind.content_bytes {
            let bytes = contents.clone();
            if let Some(mount) = by_path.get(bind.mount_path.as_str()) {
                verify_hash(&mount.source, &bytes)?;
            }
            bytes
        } else {
            let Some(mount) = by_path.get(bind.mount_path.as_str()).copied() else {
                continue;
            };
            if let Some(source_ref) = cache_volume::bind_source_ref(
                &mount.source,
                &bind.mount_path,
                persistent_volume_claims,
            ) {
                bind.source_ref =
                    source_ref.map_err(|message| err(RuntimeError::Backend(message)))?;
                continue;
            }
            match &mount.source {
                pc::MountSource::File { .. }
                | pc::MountSource::Resource { .. }
                | pc::MountSource::Secret { .. } => {
                    match resolve_blob(&mount.source, seed, store, secret_broker).await? {
                        Some(bytes) => {
                            verify_hash(&mount.source, &bytes)?;
                            bytes
                        }
                        None if mount.required => {
                            return Err(err(RuntimeError::Backend(format!(
                                "required mount `{}` did not resolve: no seed or store bytes \
                                 for its content id",
                                bind.mount_path
                            ))));
                        }
                        // An optional miss stays unrealized (forward-compat, like the seed path).
                        None => continue,
                    }
                }
                // MemoryStore is realized through its mounter, not this byte-bind loop.
                _ => continue,
            }
        };
        let original_mount_path = bind.mount_path.clone();
        let mount = by_path.get(original_mount_path.as_str()).copied();
        let directory_bind = mount.is_some_and(pc::MountRequirement::is_secret_writeback);
        let config_mount_path = if directory_bind {
            let (parent, filename) = original_mount_path.rsplit_once('/').ok_or_else(|| {
                err(RuntimeError::Backend(format!(
                    "credential mount needs an absolute file path: {original_mount_path}"
                )))
            })?;
            if parent.is_empty() || filename.is_empty() {
                return Err(err(RuntimeError::Backend(format!(
                    "credential mount needs a non-root parent and filename: {original_mount_path}"
                ))));
            }
            Some((parent.to_string(), filename.to_string()))
        } else {
            None
        };
        let staged_path = if host_bind_materialization {
            // Docker/Podman bind the host file. A writable native credential
            // receives a directory bind so the CLI may keep transient state
            // beside the credential file.
            let dir = staging_dir(&mut guard, &spec.scope)?;
            let (host_file, host_source) = if let Some((parent, filename)) = &config_mount_path {
                let config_dir =
                    dir.join(format!("credential-{}", stage_name(&original_mount_path)));
                std::fs::create_dir(&config_dir).map_err(|e| {
                    err(RuntimeError::Backend(format!("stage credential dir: {e}")))
                })?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o777))
                        .map_err(|e| {
                        err(RuntimeError::Backend(format!("secure credential dir: {e}")))
                    })?;
                }
                bind.mount_path = parent.clone();
                bind.credential_file_path = Some(original_mount_path.clone());
                (config_dir.join(filename), config_dir)
            } else {
                let host_file = dir.join(stage_name(&original_mount_path));
                (host_file.clone(), host_file)
            };
            std::fs::write(&host_file, &bytes)
                .map_err(|e| err(RuntimeError::Backend(format!("stage mount content: {e}"))))?;
            #[cfg(unix)]
            if mount.is_some_and(|mount| mount.access == pc::MountAccess::ReadWrite) {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&host_file, std::fs::Permissions::from_mode(0o666))
                    .map_err(|e| {
                        err(RuntimeError::Backend(format!("secure writable mount: {e}")))
                    })?;
            }
            bind.source_ref = host_source.to_string_lossy().into_owned();
            Some(host_file)
        } else {
            // Kubernetes projects the bytes from the plan. Preserve only the
            // logical writable-directory projection needed by the Pod builder;
            // no host path participates in this realization.
            if let Some((parent, _)) = &config_mount_path {
                bind.mount_path = parent.clone();
                bind.credential_file_path = Some(original_mount_path.clone());
            }
            None
        };
        if config_mount_path.is_some() && bind.credential_file_path.is_none() {
            bind.credential_file_path = Some(original_mount_path.clone());
        }
        if let Some(mut writeback) = mount.and_then(secret_writeback_projection) {
            writeback.staged_path = staged_path;
            secret_writebacks.push(writeback);
        }
        // 3. For the k8s tier: record the bytes so `build_pod` projects a ConfigMap — UTF-8
        // as `content` (ConfigMap `data`), otherwise as `content_bytes` (ConfigMap
        // `binaryData`). Inline/Other already carry `content`; a resolved File/Resource fills one.
        let is_secret =
            mount.is_some_and(|mount| matches!(&mount.source, pc::MountSource::Secret { .. }));
        if is_secret {
            bind.secret_content = Some(SecretBytes::new(bytes));
        } else if !had_content {
            match String::from_utf8(bytes) {
                Ok(text) => bind.content = Some(text),
                Err(e) => bind.content_bytes = Some(e.into_bytes()),
            }
        }
    }
    Ok(StagedMounts {
        guard,
        secret_writebacks,
        memory: Vec::new(),
    })
}

fn binds_of(spec: &pc::SandboxSpec) -> Vec<BindPlan> {
    spec.mounts
        .iter()
        // MemoryStore is not a host byte path; its authoritative mounter realizes it.
        .filter(|m| !matches!(m.source, pc::MountSource::MemoryStore { .. }))
        .map(|m| BindPlan {
            source_ref: mount_ref(&m.source),
            mount_path: m.mount_path.clone(),
            read_only: m.access == pc::MountAccess::ReadOnly,
            content: inline_content_of(&m.source),
            content_bytes: match &m.source {
                pc::MountSource::InlineBytes { contents, .. } => Some(contents.clone()),
                _ => None,
            },
            secret_content: None,
            secret_writeback: m.is_secret_writeback(),
            credential_file_path: None,
        })
        .collect()
}

/// The self-contained bytes of an `Inline` content-bearing mount,
/// carried in the plan so a tier without a blob store can realize it in-band — docker
/// stages it to a host file, k8s projects it as a ConfigMap. `None` for ref-backed
/// sources (their bytes live in a store the tier resolves by `source_ref`).
fn inline_content_of(source: &pc::MountSource) -> Option<String> {
    match source {
        pc::MountSource::Inline { contents } => Some(contents.clone()),
        _ => None,
    }
}

/// The memory-store mounts a spec requests, pulled out of the byte-bind set so the
/// container tier realizes each through the canonical MemoryMounter and either a
/// host bind or a runtime-native seeded volume.
fn memory_mounts_of(spec: &pc::SandboxSpec) -> Vec<MemoryMount> {
    memory_projections(spec)
        .into_iter()
        .map(|projection| MemoryMount {
            store_id: projection.store_id,
            mount_path: projection.mount_path,
            access: projection.access,
            snapshot_tar: Vec::new(),
        })
        .collect()
}

fn mount_ref(source: &pc::MountSource) -> String {
    match source {
        pc::MountSource::File { file_id, .. } => file_id.clone(),
        pc::MountSource::Resource { resource_id, .. } => resource_id.clone(),
        pc::MountSource::MemoryStore { store_id, .. } => store_id.clone(),
        pc::MountSource::Secret { reference, .. } => reference.clone(),
        // A Cache Volume is identified by its caller-owned reuse key (ADR-0056).
        pc::MountSource::CacheVolume { key, .. } => key.clone(),
        // Inline content has no host ref: the container tier binds a `source_ref` as a
        // literal path, so realizing inline bytes here needs host-file materialization
        // first (same follow-up as content-store resolution for File/Resource). Empty
        // ref = not realized on this tier yet (bwrap realizes it, see awaken-sandbox-local).
        pc::MountSource::Inline { .. } => String::new(),
        pc::MountSource::InlineBytes { .. } => String::new(),
    }
}

/// The attempt-agent command from the typed sandbox contract.
/// The provider executes it inside the Session environment; an empty command is a
/// caller error rejected fail-closed.
#[must_use]
pub fn command_of(spec: &pc::SandboxSpec) -> Vec<String> {
    spec.command.clone()
}

fn inline_env(spec: &pc::SandboxSpec) -> Vec<(String, String)> {
    spec.env
        .iter()
        .filter_map(|v| match &v.value {
            pc::EnvValue::Inline { value } => Some((v.name.clone(), value.clone())),
            // Secret refs are resolved by the broker at the runtime edge, not planned here.
            pc::EnvValue::Secret { .. } => None,
        })
        .collect()
}

/// Render a [`ContainerPlan`] from a spec + the agent command (Docker path). The
/// same egress planner used by provider creation is authoritative here, so a
/// direct planner caller cannot regain the removed fail-open allowlist path.
pub fn container_plan(
    spec: &pc::SandboxSpec,
    default_image: &str,
    command: &[String],
    forward_proxy: Option<&ForwardProxy>,
) -> Result<ContainerPlan, EgressError> {
    container_plan_with_allowlist(spec, default_image, command, forward_proxy, None)
}

fn container_plan_with_allowlist(
    spec: &pc::SandboxSpec,
    default_image: &str,
    command: &[String],
    forward_proxy: Option<&ForwardProxy>,
    allowlist_proxy: Option<&AllowlistProxy>,
) -> Result<ContainerPlan, EgressError> {
    let egress =
        egress_plan_with_allowlist(&spec.network, forward_proxy, allowlist_proxy, &spec.scope)?;
    let mut env = inline_env(spec);
    env.extend(egress.proxy_env);
    Ok(ContainerPlan {
        image: image_of(spec, default_image),
        command: command.to_vec(),
        env,
        control_services: spec.control_services.clone(),
        packages: spec.packages.clone(),
        binds: binds_of(spec),
        outputs_volume: spec.outputs_path.clone(),
        network: egress.network,
        egress_identity: egress.identity,
        requests: spec.requests.clone(),
        limits: spec.limits.clone(),
        filesystem_continuity: spec.filesystem_continuity,
        memory_mounts: memory_mounts_of(spec),
        rootfs: rootfs_of(spec, default_image),
    })
}

fn err(e: RuntimeError) -> pc::SandboxError {
    pc::SandboxError::new(e.to_string())
}

mod provider;
pub use provider::ContainerProvider;

/// A realized Session-owned container environment. The opaque agent channel is
/// returned together with the exact exec process by [`ContainerEnvironment::spawn_agent_process`].
pub struct ContainerSandbox<R: ContainerRuntime> {
    runtime: Arc<R>,
    id: String,
    container_id: String,
    outputs_path: String,
    /// Secret-free base requirements retained across attempt processes.
    base_env: Vec<pc::EnvVar>,
    control_services: std::collections::BTreeSet<SandboxControlServiceKind>,
    sandbox_control_incarnation: Option<pc::SandboxControlIncarnation>,
    control_publication: Arc<ContainerControlPublicationRegistry>,
    /// Resolution inputs retained so a capable runtime can project a later File
    /// generation through the same canonical BlobSource used at creation.
    blobs: Arc<std::collections::HashMap<String, Vec<u8>>>,
    file_store: Option<Arc<dyn pc::BlobSource>>,
    /// Frozen capability of the concrete resident Pod. Older adopted Pods do not
    /// gain a projector merely because the newly started runtime supports one.
    live_input_projection: bool,
    /// Runtime-owned incarnation evidence carried through Worker adoption.
    runtime_handle: Option<pc::ContainerContinuationHandle>,
    /// Exact independently governed paths retained only for checkpoint safety.
    continuation_excluded_paths: Vec<String>,
    /// Current V2 create-or-adopt evidence. `None` is retained only when a
    /// legacy V1 handle is adopted and must never be promoted implicitly.
    adoption_fingerprint: Option<pc::SandboxRealizationFingerprint>,
    realization_fingerprint: Option<pc::SandboxRealizationFingerprint>,
    /// Original CAS bases for copy-backed Memory mounts. Creation captures this
    /// once from the canonical MemoryMount; adoption reuses only the validated
    /// durable handle projection and never reconstructs heads from current bytes.
    memory_materializations: Vec<pc::MemoryMaterializationEvidence>,
    owned_paths: std::sync::Mutex<Vec<String>>,
    /// Exact request-bound wire handle for a restored target. Ordinary P-aware
    /// create/adopt paths retain their existing V1/V2 projection below.
    adopted_handle: Option<pc::SandboxHandle>,
    realized: Vec<pc::RealizedMount>,
    recovered: bool,
    /// Host staging dir for materialized inline-mount content, held for the container's
    /// lifetime and removed on drop (after the container is gone). `None` when the run
    /// staged nothing.
    lifecycle: Arc<ContainerCleanupState>,
}

impl<R: ContainerRuntime + 'static> ContainerSandbox<R> {
    /// Start an opaque stdio agent as an exec process in this Session environment.
    /// Repeated calls create independent attempt processes while preserving the same
    /// workspace, mounts, network policy, and durable sandbox handle.
    pub async fn spawn_agent(
        &self,
        command: pc::Command,
    ) -> Result<RuntimeAgentProcess, pc::SandboxError> {
        let command = self.materialize_command(command).await?;
        self.runtime
            .spawn_agent(&self.container_id, command)
            .await
            .map_err(err)
    }

    #[cfg(any(feature = "connection", test))]
    fn bind_scope(mut self, scope: impl Into<String>) -> Self {
        self.id = scope.into();
        self
    }

    async fn prepare_disposal(
        &self,
        effect_fence: Option<&pc::SandboxEffectFence>,
    ) -> Result<(), pc::SandboxError> {
        self.control_publication.close_for_dispose().await;
        // Writable durable credentials belong to the Session environment and
        // are harvested when that environment terminates. Managed cleanup
        // carries the root operation into the credential authority, whose
        // WAL/CAS makes response-loss replay idempotent; the local flag is only
        // an in-process fast path.
        let handle = effect_fence.map(|_| pc::Sandbox::handle(self));
        self.lifecycle
            .write_back_secrets(
                self.runtime.as_ref(),
                &self.container_id,
                effect_fence.zip(handle.as_ref()),
            )
            .await?;
        // Explicit effectless Sandbox disposal still owns its live RunV1 Memory
        // claim. Managed terminal/continuation cleanup carries an effect
        // fence and must use Host terminal-v2 reconciliation plus the explicit
        // acknowledgement port instead of replaying this stale claim.
        if effect_fence.is_none() {
            let mut memory = self.lifecycle.memory.lock().await;
            if let Some(memory) = memory.as_mut() {
                for mount in memory.iter_mut().filter(|mount| {
                    mount.native_runtime_volume
                        && mount.access == pc::MountAccess::ReadWrite
                        && !mount.writeback_prepared
                }) {
                    let files = files::read_files(self, &mount.mount_path).await?;
                    replace_memory_snapshot(&mount.host_path, &files)?;
                    mount.writeback_prepared = true;
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl<R: ContainerRuntime + 'static> ContainerEnvironment for ContainerSandbox<R> {
    fn outputs_path(&self) -> &str {
        &self.outputs_path
    }

    fn is_recovered(&self) -> bool {
        self.recovered
    }

    fn supports_live_mount_replacement(
        &self,
        previous: &[pc::MountRequirement],
        next: &[pc::MountRequirement],
    ) -> bool {
        self.supports_live_mount_replacement(previous, next)
    }

    async fn remove_live_input_path(&self, path: &str) -> Result<(), pc::SandboxError> {
        self.remove_live_input_path(path).await
    }

    fn record_owned_path(&self, path: &str) -> Result<(), pc::SandboxError> {
        let mut owned = self.owned_paths.lock().expect("owned paths lock poisoned");
        if !owned.iter().any(|current| current == path) {
            owned.push(path.to_string());
        }
        Ok(())
    }

    async fn spawn_agent_process(
        &self,
        command: pc::Command,
    ) -> Result<RuntimeAgentProcess, pc::SandboxError> {
        self.spawn_agent(command).await
    }

    async fn open_agent_channel(&self) -> Result<Box<dyn AgentChannel>, pc::SandboxError> {
        self.runtime
            .open_channel(&self.container_id)
            .await
            .map_err(err)
    }

    async fn read_files(&self, root: &str) -> Result<Vec<EnvironmentFile>, pc::SandboxError> {
        files::read_files(self, root).await
    }
}

/// Backend-erased lifecycle for never-used container capacity.
///
/// This is deliberately separate from [`ContainerEnvironmentProvider`]: every
/// container backend can create a Session environment, while only a deployment
/// that opted into a warm pool owns pre-created capacity. A composition root keeps
/// this handle long enough to prewarm before advertising readiness and to drain the
/// unused capacity during shutdown.
#[async_trait]
pub trait ContainerEnvironmentCapacity: Send + Sync {
    /// Ensure at least `target` ready, never-used containers exist for `spec`'s
    /// exact creation shape. Returns the resulting ready count. Non-poolable specs
    /// (currently any spec with mounts) return zero without creating anything.
    async fn prewarm_to(
        &self,
        spec: &pc::SandboxSpec,
        target: usize,
    ) -> Result<usize, pc::SandboxError>;

    /// Current ready, never-used capacity for one exact shape. Receipts are
    /// derived from this value so an eviction or Session checkout cannot leave a
    /// stale positive placement hint.
    fn ready_capacity(&self, spec: &pc::SandboxSpec) -> usize;

    /// Dispose unused capacity for one exact shape. This is used when the
    /// authoritative current Environment catalog no longer desires an older
    /// revision; active Session environments have already left capacity.
    async fn discard_shape(&self, spec: &pc::SandboxSpec);

    /// Stop replenishment and dispose all never-used capacity. Active Session
    /// environments have already left the pool and are not affected.
    async fn shutdown_capacity(&self);
}

struct ContainerCleanupState {
    staging: std::sync::Mutex<Option<StagingGuard>>,
    secret_writebacks: Vec<SecretWriteback>,
    secret_broker: Option<Arc<dyn pc::SecretBroker>>,
    memory: tokio::sync::Mutex<Option<Vec<StagedMemoryMount>>>,
    memory_reconciliation_ack: pc::MemoryReconciliationAck,
    writeback_done: tokio::sync::Mutex<bool>,
    remove_done: tokio::sync::Mutex<bool>,
}

impl ContainerCleanupState {
    fn recovered(
        secret_broker: Option<Arc<dyn pc::SecretBroker>>,
        staging: Option<StagingGuard>,
    ) -> Self {
        Self {
            staging: std::sync::Mutex::new(staging),
            secret_writebacks: Vec::new(),
            secret_broker,
            memory: tokio::sync::Mutex::new(None),
            memory_reconciliation_ack: pc::MemoryReconciliationAck::default(),
            writeback_done: tokio::sync::Mutex::new(true),
            remove_done: tokio::sync::Mutex::new(false),
        }
    }

    fn physical_cleanup_only() -> Self {
        Self {
            staging: std::sync::Mutex::new(None),
            secret_writebacks: Vec::new(),
            secret_broker: None,
            memory: tokio::sync::Mutex::new(None),
            memory_reconciliation_ack: pc::MemoryReconciliationAck::default(),
            writeback_done: tokio::sync::Mutex::new(true),
            remove_done: tokio::sync::Mutex::new(false),
        }
    }

    fn recovered_native(
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        secret_broker: Option<Arc<dyn pc::SecretBroker>>,
        reconstructible: bool,
    ) -> Result<Self, pc::SandboxError> {
        let secret_writebacks = spec
            .mounts
            .iter()
            .filter_map(secret_writeback_projection)
            .collect::<Vec<_>>();
        let recovered_memory = validated_recovered_memory_projections(spec, handle)?;
        let has_writable_memory = recovered_memory
            .iter()
            .any(|projection| projection.access == pc::MountAccess::ReadWrite);
        if (!secret_writebacks.is_empty() || has_writable_memory) && !reconstructible {
            return Err(pc::SandboxError::new(
                "container terminal participants depend on an unrecoverable host-bind attempt",
            ));
        }
        if !secret_writebacks.is_empty() && secret_broker.is_none() {
            return Err(pc::SandboxError::new(
                "recovered writable Secret has no credential broker",
            ));
        }
        if recovered_memory.iter().any(|projection| {
            projection.access == pc::MountAccess::ReadWrite
                && projection.write_consistency == pc::MemoryWriteConsistency::WriteThroughRequired
        }) {
            return Err(pc::SandboxError::new(
                "recovered native Memory cannot satisfy write-through consistency",
            ));
        }
        let writeback_done = secret_writebacks.is_empty();
        Ok(Self {
            staging: std::sync::Mutex::new(None),
            secret_writebacks,
            secret_broker,
            memory: tokio::sync::Mutex::new(None),
            memory_reconciliation_ack: pc::MemoryReconciliationAck::default(),
            writeback_done: tokio::sync::Mutex::new(writeback_done),
            remove_done: tokio::sync::Mutex::new(false),
        })
    }

    async fn write_back_secrets<R: ContainerRuntime>(
        &self,
        runtime: &R,
        container_id: &str,
        effect: Option<(&pc::SandboxEffectFence, &pc::SandboxHandle)>,
    ) -> Result<(), pc::SandboxError> {
        let mut done = self.writeback_done.lock().await;
        if *done {
            return Ok(());
        }
        if !self.secret_writebacks.is_empty() {
            let broker = self.secret_broker.as_ref().ok_or_else(|| {
                pc::SandboxError::new("durable writable Secret has no credential broker")
            })?;
            for item in &self.secret_writebacks {
                let bytes = match runtime
                    .read_live_file(container_id, &item.mount_path)
                    .await
                    .map_err(err)?
                {
                    Some(bytes) => bytes,
                    None => {
                        let staged_path = item.staged_path.as_ref().ok_or_else(|| {
                            pc::SandboxError::new(
                                "native container credential harvest returned no live bytes",
                            )
                        })?;
                        std::fs::read(staged_path).map_err(|e| {
                            pc::SandboxError::new(format!("read refreshed credential file: {e}"))
                        })?
                    }
                };
                match effect {
                    Some((authorization, handle)) => {
                        let writeback = pc::SecretWritebackEffect::new(
                            item.reference.clone(),
                            handle.container_physical_incarnation()?,
                            authorization.clone(),
                        )?;
                        broker.write_back_for_effect(&writeback, bytes).await?
                    }
                    None => broker.write_back(&item.reference, bytes).await?,
                }
            }
        }
        *done = true;
        Ok(())
    }

    async fn dispose_once<R: ContainerRuntime>(
        &self,
        runtime: &R,
        container_id: &str,
        runtime_handle: Option<&pc::ContainerContinuationHandle>,
    ) -> Result<(), pc::SandboxError> {
        let mut done = self.remove_done.lock().await;
        if !*done {
            runtime
                .remove_with_handle(container_id, runtime_handle)
                .await
                .map_err(err)?;
            self.release_dependencies().await?;
            *done = true;
        }
        Ok(())
    }

    async fn dispose_once_for_effect<R: ContainerRuntime>(
        &self,
        runtime: &R,
        expectation: ContainerObservationExpectation<'_>,
        authorization: &pc::SandboxDisposalAuthorization,
    ) -> Result<(), pc::SandboxError> {
        let mut done = self.remove_done.lock().await;
        if !*done {
            runtime
                .dispose_authorized(expectation, authorization)
                .await
                .map_err(err)?;
            self.release_dependencies().await?;
            *done = true;
        }
        Ok(())
    }

    async fn release_dependencies(&self) -> Result<(), pc::SandboxError> {
        let mut memory = self.memory.lock().await;
        if let Some(mounts) = memory.as_ref() {
            for mount in mounts {
                mount.handle.teardown().await?;
            }
            *memory = None;
        }
        drop(memory);
        if let Some(staging) = self.staging.lock().expect("staging mutex poisoned").take() {
            staging.remove();
        }
        Ok(())
    }
}

#[async_trait]
impl<R: ContainerRuntime + 'static> pc::Sandbox for ContainerSandbox<R> {
    fn id(&self) -> &str {
        &self.id
    }

    fn handle(&self) -> pc::SandboxHandle {
        if let Some(handle) = &self.adopted_handle {
            return handle.clone();
        }
        let previous = pc::ContainerSandboxHandleV1 {
            container_id: self.container_id.clone(),
            outputs_path: self.outputs_path.clone(),
            base_env: self.base_env.clone(),
            live_input_projection: self.live_input_projection,
            continuation_excluded_paths: self.continuation_excluded_paths.clone(),
            runtime_handle: self.runtime_handle.clone(),
            sandbox_control_incarnation: self.sandbox_control_incarnation.clone(),
            control_services: self.control_services.clone(),
        };
        match (&self.adoption_fingerprint, &self.realization_fingerprint) {
            (Some(adoption_fingerprint), Some(realization_fingerprint)) => {
                pc::SandboxHandle::container_v2(
                    &self.id,
                    pc::ContainerSandboxHandleV2 {
                        previous,
                        adoption_fingerprint: adoption_fingerprint.clone(),
                        realization_fingerprint: realization_fingerprint.clone(),
                        owned_paths: self
                            .owned_paths
                            .lock()
                            .expect("owned paths lock poisoned")
                            .clone(),
                    },
                )
                .with_memory_materializations(self.memory_materializations.clone())
                .expect("container Memory materializations were validated before physical create")
            }
            // V1 recovery stays V1. A later Resource reservation cannot use a
            // raw container locator to fabricate current realization evidence.
            (None, None) => {
                debug_assert!(self.memory_materializations.is_empty());
                pc::SandboxHandle::container(&self.id, previous)
            }
            _ => unreachable!("container realization evidence is emitted atomically"),
        }
    }

    async fn spawn(
        &self,
        command: pc::Command,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        let command = self.materialize_command(command).await?;
        self.runtime
            .spawn(&self.container_id, command)
            .await
            .map_err(err)
    }

    async fn attach(
        &self,
        req: pc::MountRequirement,
    ) -> Result<pc::RealizedMount, pc::SandboxError> {
        self.attach_live_input(req).await
    }

    async fn artifacts(&self) -> Result<Vec<pc::Artifact>, pc::SandboxError> {
        self.runtime
            .artifacts(&self.container_id)
            .await
            .map_err(err)
    }

    async fn read_artifact(&self, id: &str) -> Result<Vec<u8>, pc::SandboxError> {
        self.runtime
            .read_artifact(&self.container_id, id)
            .await
            .map_err(err)
    }

    fn realized(&self) -> &[pc::RealizedMount] {
        &self.realized
    }

    async fn process(
        &self,
        process_id: &str,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        self.runtime
            .process(&self.container_id, process_id)
            .await
            .map_err(err)
    }

    async fn status(&self) -> Result<pc::SandboxStatus, pc::SandboxError> {
        match self
            .runtime
            .inspect(&self.container_id)
            .await
            .map_err(err)?
        {
            ContainerState::Provisioning => Ok(pc::SandboxStatus::Provisioning),
            ContainerState::Running => Ok(pc::SandboxStatus::Ready),
            ContainerState::Gone => Ok(pc::SandboxStatus::Terminated),
        }
    }

    async fn renew_lease(&self) -> Result<(), pc::SandboxError> {
        self.runtime
            .touch_lease(&self.container_id)
            .await
            .map_err(err)
    }

    async fn acknowledge_memory_reconciliation(
        &self,
        effect_fence: &pc::SandboxEffectFence,
        complete_materializations: &[pc::MemoryMaterializationEvidence],
    ) -> Result<(), pc::SandboxError> {
        // `SandboxHandle` is the sole durable provider projection and
        // canonicalizes this slice. Staging order must never become a second
        // evidence ordering contract.
        let handle = self.handle();
        let expected_materializations = handle.memory_materializations()?.unwrap_or(&[]);
        let mut memory = self.lifecycle.memory.lock().await;
        self.lifecycle.memory_reconciliation_ack.acknowledge(
            effect_fence,
            expected_materializations,
            complete_materializations,
            || {
                if let Some(mounts) = memory.as_mut() {
                    if mounts
                        .iter()
                        .filter_map(|mount| mount.materialization.as_ref())
                        .any(|materialization| !expected_materializations.contains(materialization))
                    {
                        return Err(pc::SandboxError::new(
                            "Container CopyMount guard differs from its durable Memory evidence",
                        ));
                    }
                    // Dropping a Copy guard is the process-local acknowledgement;
                    // do not call RunV1 teardown after Host terminal-v2 CAS.
                    // FUSE remains mounted and staging remains owned until the
                    // runtime proves physical removal.
                    mounts.retain(|mount| mount.materialization.is_none());
                }
                Ok(())
            },
        )
    }

    async fn dispose(&self) -> Result<(), pc::SandboxError> {
        self.prepare_disposal(None).await?;
        self.lifecycle
            .dispose_once(
                self.runtime.as_ref(),
                &self.container_id,
                self.runtime_handle.as_ref(),
            )
            .await
    }

    async fn prepare_disposal_for_effect(
        &self,
        effect_fence: &pc::SandboxEffectFence,
    ) -> Result<pc::SandboxEffectFence, pc::SandboxError> {
        if self.adoption_fingerprint.is_none() || self.realization_fingerprint.is_none() {
            return Err(pc::SandboxError::new(
                "legacy container handle cannot authorize fenced physical disposal",
            ));
        }
        let handle = self.handle();
        handle.container_physical_incarnation()?;
        let expected_materializations = handle.memory_materializations()?.unwrap_or(&[]);
        // This is the provider-local data-preparation gate. Missing, stale,
        // foreign, or predecessor acknowledgements have zero Secret and
        // physical effects. Successful Secret write-back remains replayable,
        // but no runtime removal (and therefore no Kubernetes cleanup
        // finalizer) is reachable from this method.
        self.lifecycle
            .memory_reconciliation_ack
            .require_for_disposal(expected_materializations, effect_fence)?;
        self.prepare_disposal(Some(effect_fence)).await?;
        // Container/Kubernetes install their exact physical gate only at the
        // later disposal edge. This source-preparation boundary therefore
        // returns the caller's exact fence without claiming marker evidence.
        Ok(effect_fence.clone())
    }

    async fn dispose_for_effect(
        &self,
        authorization: &pc::SandboxDisposalAuthorization,
    ) -> Result<(), pc::SandboxError> {
        authorization.validate()?;
        if self.adoption_fingerprint.is_none() || self.realization_fingerprint.is_none() {
            return Err(pc::SandboxError::new(
                "legacy container handle cannot authorize fenced physical disposal",
            ));
        }
        // The aggregate persisted the separate disposal-preparation receipt
        // before invoking this physical edge. Rebuild only the immutable
        // observation expectation here: Secret, Memory, Artifact, checkpoint,
        // and Hand I/O must not be repeated after that persistence boundary.
        let handle = self.handle();
        handle.container_physical_incarnation()?;
        let expectation = ContainerObservationExpectation::from_handle_for_effect(
            &handle,
            authorization.effect_fence(),
        )?;
        self.lifecycle
            .dispose_once_for_effect(self.runtime.as_ref(), expectation, authorization)
            .await
    }
}

/// Remote-tier transport over `awaken-connection` (TCP dial + reverse dial). Gated
/// behind the `connection` feature so the default build pulls no network stack.
#[cfg(feature = "connection")]
pub mod net;

/// Real Docker backend (bollard). Gated behind the `docker` feature; compile-verified
/// here, running requires a Docker daemon.
#[cfg(feature = "docker")]
pub mod docker;

#[cfg(feature = "k8s")]
pub mod k8s;
/// Real Kubernetes backend (kube). Gated behind the `k8s` feature; compile-verified
/// here, running requires a cluster.
#[cfg(feature = "k8s")]
mod k8s_package_image;
/// Versioned, typed, read-only package-realization evidence shared by the sole
/// Kubernetes emitter and pinned composing consumers.
#[cfg(feature = "k8s")]
pub mod k8s_package_realization;

/// Rootless Podman backend (daemonless CLI fork-exec). Gated behind the `podman`
/// feature; compile-verified here, running requires the `podman` binary.
#[cfg(feature = "podman")]
pub mod podman;

/// Warm container pool (pre-provisioned reusable capacity): cold-start + reuse. Gated
/// on `connection` (present under every real backend), which brings the tokio runtime
/// the pool's off-path replenish spawns onto.
#[cfg(any(feature = "connection", test))]
pub mod pool;
#[cfg(any(feature = "connection", test))]
pub use pool::WarmContainerPool;

#[cfg(test)]
mod tests;
