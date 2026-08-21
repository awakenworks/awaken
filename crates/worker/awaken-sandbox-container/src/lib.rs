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
use std::sync::Arc;

mod cache_volume;
mod environment_owned;
use environment_owned::EnvironmentOwnedProcess;
mod cgroup;
mod egress;
mod files;
mod live_inputs;
mod packages;
mod podman_plan;
mod process_env;
mod provider_contract;
use process_env::environment_keepalive_command;
pub use process_env::runtime_configuration_homes;
mod recovery;
mod resident_hand;
mod runtime;
mod secret;
mod writable;
pub use cgroup::CgroupCaps;
pub use egress::{
    AllowlistCapability, AllowlistProxy, EgressError, EgressRealization, ForwardProxy, NetworkMode,
    egress_plan, egress_plan_with_allowlist, normalize_hostname,
};
pub use live_inputs::{LIVE_INPUTS_ROOT, live_input_relative_path};
pub use packages::package_containerfile;
pub use podman_plan::{RootfsError, RootfsPlan, podman_run_argv, rootfs_plan};
use podman_plan::{image_of, rootfs_of};
pub use provider_contract::{
    ContainerEnvironment, ContainerEnvironmentAdoption, ContainerEnvironmentProvider,
    EnvironmentFile,
};
pub use resident_hand::ResidentHandConfig;
pub use runtime::{
    ContainerRuntime, ContainerState, K8sContinuationVolume, MemoryMount, PackageImageProvisioner,
    RuntimeAgentProcess, RuntimeError,
};
use runtime::{allowlist_capability_advertised, container_capabilities};
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
    pub packages: pc::PackageRequirements,
    pub binds: Vec<BindPlan>,
    /// Out-of-band outputs volume mount path (artifacts leave via the volume).
    pub outputs_volume: String,
    pub network: NetworkMode,
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
pub(crate) struct StagingGuard(std::path::PathBuf);

impl StagingGuard {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug)]
struct SecretWriteback {
    reference: String,
    staged_path: std::path::PathBuf,
    mount_path: String,
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
    native_runtime_volume: bool,
    writeback_prepared: bool,
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

async fn stage_memory_binds(
    spec: &pc::SandboxSpec,
    plan: &mut ContainerPlan,
    staged: &mut StagedMounts,
    mounter: Option<Arc<dyn pc::MemoryMounter>>,
    native_runtime_volume: bool,
) -> Result<(), pc::SandboxError> {
    let memory: Vec<_> = spec
        .mounts
        .iter()
        .filter_map(|mount| match &mount.source {
            pc::MountSource::MemoryStore {
                store_id,
                materialization_reference,
                write_consistency,
            } => Some((
                mount,
                store_id,
                materialization_reference,
                write_consistency,
            )),
            _ => None,
        })
        .collect();
    if memory.is_empty() {
        return Ok(());
    }
    let mounter = mounter
        .ok_or_else(|| pc::SandboxError::new("container MemoryStore mount has no MemoryMounter"))?;
    let root = staging_dir(&mut staged.guard, &spec.scope)?;
    for (mount, store_id, materialization_reference, write_consistency) in memory {
        let host_path = root.join(format!("memory-{}", stage_name(&mount.mount_path)));
        let handle = mounter
            .mount(
                materialization_reference.as_deref().unwrap_or(store_id),
                &host_path,
                mount.access,
            )
            .await?;
        if *write_consistency == pc::MemoryWriteConsistency::WriteThroughRequired
            && handle.realization() != pc::Realization::Fuse
        {
            handle.teardown().await;
            return Err(pc::SandboxError::new(
                "MemoryStore mount requires write-through FUSE realization",
            ));
        }
        if native_runtime_volume && handle.realization() != pc::Realization::Copy {
            handle.teardown().await;
            return Err(pc::SandboxError::new(
                "remote container Memory volume requires a copy-capable MemoryMounter",
            ));
        }
        #[cfg(unix)]
        if let Err(error) = make_memory_tree_accessible(&host_path, mount.access) {
            handle.teardown().await;
            return Err(error);
        }
        if native_runtime_volume {
            let snapshot_tar = match memory_snapshot_tar(&host_path) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    handle.teardown().await;
                    return Err(error);
                }
            };
            let planned = plan
                .memory_mounts
                .iter_mut()
                .find(|planned| planned.mount_path == mount.mount_path)
                .ok_or_else(|| pc::SandboxError::new("Memory mount disappeared from plan"));
            let planned = match planned {
                Ok(planned) => planned,
                Err(error) => {
                    handle.teardown().await;
                    return Err(error);
                }
            };
            planned.snapshot_tar = snapshot_tar;
            planned.access = mount.access;
        } else {
            plan.binds.push(BindPlan {
                source_ref: host_path.to_string_lossy().into_owned(),
                mount_path: mount.mount_path.clone(),
                read_only: mount.access == pc::MountAccess::ReadOnly,
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
            mount_path: mount.mount_path.clone(),
            access: mount.access,
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
    *guard = Some(StagingGuard(d));
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
/// content ships as-is; reference sources resolve through [`resolve_blob`]. Bytes
/// are staged for Docker/Podman or Kubernetes; required misses fail closed.
async fn resolve_and_stage(
    spec: &pc::SandboxSpec,
    binds: &mut [BindPlan],
    seed: &std::collections::HashMap<String, Vec<u8>>,
    store: &Option<Arc<dyn pc::BlobSource>>,
    secret_broker: &Option<Arc<dyn pc::SecretBroker>>,
    persistent_volume_claims: bool,
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
        // 2. Stage the bytes to a host file the docker/podman tier binds. A native
        // credential gets a whole writable config-directory bind: CLIs keep transient
        // state beside auth.json, and Docker cannot write a nested file bind beneath a
        // tmpfs parent. Only the credential file is harvested below.
        let dir = staging_dir(&mut guard, &spec.scope)?;
        let original_mount_path = bind.mount_path.clone();
        let mount = by_path.get(original_mount_path.as_str()).copied();
        let directory_bind = mount.is_some_and(pc::MountRequirement::is_secret_writeback);
        let (host_file, host_config_dir, config_mount_path) = if directory_bind {
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
            let config_dir = dir.join(format!("credential-{}", stage_name(&original_mount_path)));
            std::fs::create_dir(&config_dir)
                .map_err(|e| err(RuntimeError::Backend(format!("stage credential dir: {e}"))))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                // The random outer directory is 0700; this inner directory may be writable
                // by the image-defined UID without exposing its contents to host users.
                std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o777))
                    .map_err(|e| {
                        err(RuntimeError::Backend(format!("secure credential dir: {e}")))
                    })?;
            }
            (
                config_dir.join(filename),
                Some(config_dir),
                Some(parent.to_string()),
            )
        } else {
            (dir.join(stage_name(&original_mount_path)), None, None)
        };
        std::fs::write(&host_file, &bytes)
            .map_err(|e| err(RuntimeError::Backend(format!("stage mount content: {e}"))))?;
        #[cfg(unix)]
        if mount.is_some_and(|mount| mount.access == pc::MountAccess::ReadWrite) {
            use std::os::unix::fs::PermissionsExt;
            // Docker/Podman images deliberately run as non-root with an image-defined UID
            // that is not known to the host. The private 0700 parent prevents host users
            // from reaching this per-session file; 0666 lets the sandboxed UID honor the
            // neutral ReadWrite contract (resource/memory write-back and OAuth refresh).
            std::fs::set_permissions(&host_file, std::fs::Permissions::from_mode(0o666))
                .map_err(|e| err(RuntimeError::Backend(format!("secure writable mount: {e}"))))?;
        }
        bind.source_ref = host_config_dir
            .as_ref()
            .unwrap_or(&host_file)
            .to_string_lossy()
            .into_owned();
        if let Some(config_mount_path) = config_mount_path {
            bind.mount_path = config_mount_path;
            bind.credential_file_path = Some(original_mount_path.clone());
        }
        if let Some(mount) = mount
            && mount.is_secret_writeback()
            && let pc::MountSource::Secret { reference, .. } = &mount.source
        {
            secret_writebacks.push(SecretWriteback {
                reference: reference.clone(),
                staged_path: host_file.clone(),
                mount_path: mount.mount_path.clone(),
            });
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
    spec.mounts
        .iter()
        .filter_map(|m| match &m.source {
            pc::MountSource::MemoryStore { store_id, .. } => Some(MemoryMount {
                store_id: store_id.clone(),
                mount_path: m.mount_path.clone(),
                access: m.access,
                snapshot_tar: Vec::new(),
            }),
            _ => None,
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
        packages: spec.packages.clone(),
        binds: binds_of(spec),
        outputs_volume: spec.outputs_path.clone(),
        network: egress.network,
        requests: spec.requests.clone(),
        limits: spec.limits.clone(),
        filesystem_continuity: spec.filesystem_continuity,
        memory_mounts: memory_mounts_of(spec),
        rootfs: rootfs_of(spec, default_image),
    })
}

/// Stable label every managed Sandbox container/pod carries. The wire value is
/// retained for compatibility with existing resources; it is discovery evidence,
/// never disposal authorization.
pub(crate) const MANAGED_SANDBOX_LABEL: &str = "awaken.sandbox";
/// Identifies the runtime incarnation that currently owns a container. It fences
/// realization/adoption only and cannot authorize garbage collection.
#[cfg(any(feature = "docker", feature = "podman", feature = "k8s"))]
pub(crate) const RUNTIME_OWNER_LABEL: &str = "awaken.sandbox.owner";

pub(crate) fn runtime_owner_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}-{epoch}-{sequence}", std::process::id())
}

/// A daemon-global container name. Session/thread ids are only unique inside one
/// host, while Docker and Podman names share a daemon namespace across hosts and CI
/// processes. Prefix the readable scope with the runtime-owner fingerprint so two
/// valid `sesn_0` executions cannot collide. The durable sandbox identity remains
/// the original scope; this is only an adapter-local runtime name.
#[cfg(any(feature = "docker", feature = "podman"))]
pub(crate) fn runtime_container_name(owner_id: &str, scope: &str) -> String {
    let owner = blake3::hash(owner_id.as_bytes()).to_hex();
    let scope = stage_name(scope);
    let scope = &scope[..scope.len().min(80)];
    format!("awaken-{}-{scope}", &owner[..16])
}

fn err(e: RuntimeError) -> pc::SandboxError {
    pc::SandboxError::new(e.to_string())
}

// ── Provider + Sandbox over the port ────────────────────────────────────────────

/// Realizes [`pc::Sandbox`]es on a [`ContainerRuntime`].
pub struct ContainerProvider<R: ContainerRuntime> {
    runtime: Arc<R>,
    package_provisioner: Option<Arc<dyn PackageImageProvisioner>>,
    default_image: String,
    /// Optional connectivity proxy for unrestricted traffic. It is never treated
    /// as network-policy enforcement.
    forward_proxy: Option<ForwardProxy>,
    /// Capability issuer plus proxy coordinate. Presence is not enough to
    /// advertise support: the runtime must independently attest that direct
    /// workload egress cannot bypass this proxy.
    allowlist_proxy: Option<AllowlistProxy>,
    /// In-memory blob seed for `File`/`Resource`/`Secret` mounts (keyed by content id),
    /// consulted before the store — the test/seed path, mirroring `LocalProvider`.
    blobs: Arc<std::collections::HashMap<String, Vec<u8>>>,
    /// The injected content-addressed store consulted after the seed. The worker tier
    /// links no durable store (A-G17); the composition root injects an adapter over the
    /// resources-tier content store, so a `File`/`Resource` id resolves to real bytes.
    file_store: Option<Arc<dyn pc::BlobSource>>,
    /// Bidirectional broker used only for `MountSource::Secret`; durable writable
    /// mounts are committed through it after the agent process exits.
    secret_broker: std::sync::RwLock<Option<Arc<dyn pc::SecretBroker>>>,
    /// Neutral MemoryStore projection injected by the composition root. Interior
    /// mutability lets an already-shared provider receive the platform adapter.
    memory_mounter: std::sync::RwLock<Option<Arc<dyn pc::MemoryMounter>>>,
    resident_hand: Option<ResidentHandConfig>,
}

impl<R: ContainerRuntime + 'static> ContainerProvider<R> {
    async fn probe_runtime_ready(&self) -> Result<(), pc::SandboxError> {
        self.runtime.probe_ready().await.map_err(err)
    }

    pub fn new(runtime: Arc<R>, default_image: impl Into<String>) -> Self {
        Self {
            runtime,
            package_provisioner: None,
            default_image: default_image.into(),
            forward_proxy: None,
            allowlist_proxy: None,
            blobs: Arc::new(std::collections::HashMap::new()),
            file_store: None,
            secret_broker: std::sync::RwLock::new(None),
            memory_mounter: std::sync::RwLock::new(None),
            resident_hand: None,
        }
    }

    /// Make the existing Hand the Session container's resident process. This is
    /// mutually exclusive with attached-exec Hand launch at the Runtime Host.
    #[must_use]
    pub fn with_resident_hand(mut self, config: ResidentHandConfig) -> Self {
        self.resident_hand = Some(config);
        self
    }

    pub fn install_memory_mounter(&self, mounter: Arc<dyn pc::MemoryMounter>) {
        *self
            .memory_mounter
            .write()
            .expect("container memory mounter lock poisoned") = Some(mounter);
    }

    /// Configure a conventional forward proxy for unrestricted traffic.
    #[must_use]
    pub fn with_forward_proxy(mut self, proxy: ForwardProxy) -> Self {
        self.forward_proxy = Some(proxy);
        self
    }

    #[must_use]
    pub fn with_allowlist_proxy(mut self, proxy: AllowlistProxy) -> Self {
        self.allowlist_proxy = Some(proxy);
        self
    }

    /// Use an independent image builder/publisher. This is required when the
    /// execution runtime cannot build images itself (for example Kubernetes).
    #[must_use]
    pub fn with_package_provisioner(
        mut self,
        provisioner: Arc<dyn PackageImageProvisioner>,
    ) -> Self {
        self.package_provisioner = Some(provisioner);
        self
    }

    /// Register bytes a `File`/`Resource`/`Secret` mount can resolve to by content id
    /// (test/seed helper, mirroring `LocalProvider::with_blob`).
    #[must_use]
    pub fn with_blob(mut self, id: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        Arc::make_mut(&mut self.blobs).insert(id.into(), bytes.into());
        self
    }

    /// Inject the content-addressed store consulted after the seed map, so `File` /
    /// `Resource` / `Secret` mounts resolve their bytes by id at `create` (A-G17: the
    /// provider names no durable store; it holds only this `BlobSource` port).
    #[must_use]
    pub fn with_blob_source(mut self, store: Arc<dyn pc::BlobSource>) -> Self {
        self.file_store = Some(store);
        self
    }

    #[must_use]
    pub fn with_secret_broker(self, broker: Arc<dyn pc::SecretBroker>) -> Self {
        self.install_secret_broker(broker);
        self
    }

    pub fn install_secret_broker(&self, broker: Arc<dyn pc::SecretBroker>) {
        *self
            .secret_broker
            .write()
            .expect("container secret broker lock poisoned") = Some(broker);
    }

    /// Realize a Session-owned container environment. The trait `create` boxes this.
    pub async fn create_container(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<ContainerSandbox<R>, pc::SandboxError> {
        // Fail closed against our capabilities before touching the runtime.
        pc::prepare_environment(
            spec,
            &container_capabilities(
                self.runtime.enforces_network_none(),
                allowlist_capability_advertised(
                    self.runtime.enforces_network_allowlist(),
                    self.allowlist_proxy.is_some(),
                ),
                self.runtime.supports_package_provisioning() || self.package_provisioner.is_some(),
            ),
        )
        .map_err(|e| err(RuntimeError::Backend(e.to_string())))?;
        if spec
            .mounts
            .iter()
            .any(pc::MountRequirement::is_secret_writeback)
            && !self.runtime.supports_secret_writeback()
        {
            return Err(err(RuntimeError::Backend(
                "this container runtime cannot persist a writable credential-file mount"
                    .to_string(),
            )));
        }

        // A Session owns one live environment. Attached mode retains the legacy
        // keepalive; resident mode runs the existing Hand as PID 1 so Worker
        // replacement can reopen its provider-owned channel without a second
        // process-placement path.
        let command = self
            .resident_hand
            .as_ref()
            .map_or_else(environment_keepalive_command, ResidentHandConfig::command);
        let mut plan = container_plan_with_allowlist(
            spec,
            &self.default_image,
            &command,
            self.forward_proxy.as_ref(),
            self.allowlist_proxy.as_ref(),
        )
        .map_err(|e| err(RuntimeError::Backend(e.to_string())))?;
        if let Some(hand) = &self.resident_hand {
            process_env::bind_resident_process_environment(&mut plan.env, &spec.outputs_path);
            plan.env
                .push(("AWAKEN_HAND_LEDGER_DIR".into(), hand.ledger_dir.clone()));
            plan.env.push((
                "AWAKEN_HAND_LEDGER_MAX_ENTRIES".into(),
                hand.ledger_max_entries.to_string(),
            ));
            plan.env.push((
                "AWAKEN_HAND_MAX_CONNECTIONS".into(),
                hand.max_connections.to_string(),
            ));
        }
        if !plan.packages.is_empty() {
            let base_image = match &plan.rootfs {
                RootfsPlan::Image(reference) => reference.clone(),
                RootfsPlan::HostUserland => plan.image.clone(),
                _ => {
                    return Err(err(RuntimeError::Backend(
                        "package provisioning requires an OCI image rootfs".into(),
                    )));
                }
            };
            plan.image = if let Some(provisioner) = &self.package_provisioner {
                provisioner
                    .prepare_package_image(&base_image, &plan.packages, &spec.network)
                    .await
                    .map_err(err)?
            } else {
                self.runtime
                    .prepare_package_image(&base_image, &plan.packages, &spec.network)
                    .await
                    .map_err(err)?
            };
            plan.rootfs = RootfsPlan::Image(plan.image.clone());
        }
        // Resolve + materialize each mount's bytes: self-contained content (codex config,
        // ADR-0038 resources) ships in the plan; File/Resource/Secret resolve by id through
        // the seed then the injected BlobSource, hash-verified. Bytes are staged to a host
        // dir (bound by docker/podman) and recorded as `content` (projected by the k8s
        // ConfigMap path) — kept alive by the sandbox for the container's lifetime.
        let secret_broker = self
            .secret_broker
            .read()
            .expect("container secret broker lock poisoned")
            .clone();
        let mut staging = resolve_and_stage(
            spec,
            &mut plan.binds,
            &self.blobs,
            &self.file_store,
            &secret_broker,
            self.runtime.uses_persistent_volume_claims(),
        )
        .await?;
        if self.runtime.uses_host_live_input_bind() {
            live_inputs::stage_host_projection(&spec.scope, &mut plan.binds, &mut staging.guard)?;
        }
        let native_memory = self.runtime.has_native_memory_mounts();
        let mounter = self
            .memory_mounter
            .read()
            .expect("container memory mounter lock poisoned")
            .clone();
        stage_memory_binds(spec, &mut plan, &mut staging, mounter, native_memory).await?;
        let container_id = match self.runtime.create(&spec.scope, &plan).await {
            Ok(id) => id,
            Err(error) => {
                for mount in staging.memory.drain(..) {
                    mount.handle.teardown().await;
                }
                return Err(err(error));
            }
        };
        let runtime_handle = match self.runtime.handle_extra(&container_id).await {
            Ok(evidence) => evidence,
            Err(error) => {
                let _ = self.runtime.remove(&container_id).await;
                return Err(err(error));
            }
        };
        // Report each mount's realization: a byte mount is a Bind and a Memory
        // store is the canonical mounter's copy projection. Built from spec.mounts
        // directly because native volumes do not align with the byte-bind list.
        let realized = spec
            .mounts
            .iter()
            .map(|m| pc::RealizedMount {
                mount_id: m.mount_id.clone(),
                mount_path: m.mount_path.clone(),
                access: m.access,
                realization: match m.source {
                    // Remote container memory is seeded and harvested as a bounded
                    // copy through the canonical MemoryMounter.
                    pc::MountSource::MemoryStore { .. } => pc::Realization::Copy,
                    _ => pc::Realization::Bind,
                },
                content_hash: None,
            })
            .collect();
        Ok(ContainerSandbox {
            runtime: self.runtime.clone(),
            id: spec.scope.clone(),
            container_id,
            outputs_path: spec.outputs_path.clone(),
            base_env: spec.env.clone(),
            blobs: self.blobs.clone(),
            file_store: self.file_store.clone(),
            live_input_projection: self.runtime.supports_live_input_projection(),
            runtime_handle,
            continuation_excluded_paths: spec
                .mounts
                .iter()
                .map(|mount| mount.mount_path.clone())
                .collect(),
            realized,
            recovered: false,
            lifecycle: Arc::new(ContainerCleanupState {
                staging: std::sync::Mutex::new(staging.guard),
                secret_writebacks: staging.secret_writebacks,
                secret_broker,
                memory: tokio::sync::Mutex::new(Some(staging.memory)),
                writeback_done: tokio::sync::Mutex::new(false),
                remove_done: tokio::sync::Mutex::new(false),
            }),
        })
    }

    /// Re-adopt a concrete long-lived environment from its durable handle.
    pub async fn adopt_container(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<ContainerSandbox<R>, pc::SandboxError> {
        let payload = recovery::decode_handle(handle)?;
        let container_id = payload.container_id;
        if self.runtime.inspect(&container_id).await.map_err(err)? == ContainerState::Gone {
            return Err(err(RuntimeError::NotFound(container_id)));
        }
        Ok(ContainerSandbox {
            runtime: self.runtime.clone(),
            id: handle.sandbox_id.clone(),
            container_id,
            outputs_path: payload.outputs_path,
            base_env: payload.base_env,
            blobs: self.blobs.clone(),
            file_store: self.file_store.clone(),
            live_input_projection: payload.live_input_projection,
            runtime_handle: payload.runtime_handle,
            continuation_excluded_paths: payload.continuation_excluded_paths,
            realized: Vec::new(),
            recovered: true,
            lifecycle: Arc::new(ContainerCleanupState::completed(
                self.secret_broker
                    .read()
                    .expect("container secret broker lock poisoned")
                    .clone(),
            )),
        })
    }
}

/// A running agent exec, handed to the host's ACP agent-channel source: the duplex
/// channel, the exact exec process handle, and the environment handle for reattach.
pub struct AgentContainerSession {
    pub channel: Box<dyn AgentChannel>,
    pub process: Box<dyn pc::ProcessHandle>,
    pub handle: pc::SandboxHandle,
}

/// Object-safe container seam for the host: realize a Session environment, execute
/// the ACP agent inside it, and open its channel, with the backend chosen
/// behind the `dyn` by **worker config** — so one host binary drives whichever backend
/// a given worker is configured for. The counterpart of the Workdir/namespace
/// `spawn_agent` path, for a user-supplied container image.
#[async_trait]
pub trait AgentContainerProvider: Send + Sync {
    /// Create the container from the typed image and command in `spec` and open its
    /// ACP channel, returning the channel + process handle for one run.
    async fn open_agent(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<AgentContainerSession, pc::SandboxError>;
}

#[async_trait]
impl<R: ContainerRuntime + 'static> AgentContainerProvider for ContainerProvider<R> {
    async fn open_agent(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<AgentContainerSession, pc::SandboxError> {
        let environment: Arc<dyn ContainerEnvironment> =
            Arc::new(self.create_container(spec).await?);
        let argv = command_of(spec);
        if argv.is_empty() {
            return Err(pc::SandboxError::new("agent command argv is empty"));
        }
        let handle = environment.handle();
        let RuntimeAgentProcess { process, channel } = environment
            .spawn_agent_process(pc::Command {
                argv,
                cwd: String::new(),
                env: Vec::new(),
                stdio: pc::Stdio::Piped,
            })
            .await?;
        Ok(AgentContainerSession {
            channel,
            process: Box::new(EnvironmentOwnedProcess {
                inner: process,
                environment,
            }),
            handle,
        })
    }
}

#[async_trait]
impl<R: ContainerRuntime + 'static> ContainerEnvironmentProvider for ContainerProvider<R> {
    fn sandbox_capabilities(&self) -> pc::SandboxCapabilities {
        container_capabilities(
            self.runtime.enforces_network_none(),
            allowlist_capability_advertised(
                self.runtime.enforces_network_allowlist(),
                self.allowlist_proxy.is_some(),
            ),
            self.runtime.supports_package_provisioning() || self.package_provisioner.is_some(),
        )
    }

    fn install_memory_mounter(&self, mounter: Arc<dyn pc::MemoryMounter>) {
        self.install_memory_mounter(mounter);
    }

    fn install_secret_broker(&self, broker: Arc<dyn pc::SecretBroker>) {
        self.install_secret_broker(broker);
    }

    async fn probe_ready(&self) -> Result<(), pc::SandboxError> {
        self.probe_runtime_ready().await
    }

    async fn create_environment(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError> {
        Ok(Arc::new(self.create_container(spec).await?))
    }

    async fn adopt_environment(
        &self,
        adoption: ContainerEnvironmentAdoption<'_>,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError> {
        Ok(Arc::new(self.adopt_container(adoption.handle).await?))
    }
}

#[async_trait]
impl<R: ContainerRuntime + 'static> pc::SandboxProvider for ContainerProvider<R> {
    fn capabilities(&self) -> pc::SandboxCapabilities {
        container_capabilities(
            self.runtime.enforces_network_none(),
            allowlist_capability_advertised(
                self.runtime.enforces_network_allowlist(),
                self.allowlist_proxy.is_some(),
            ),
            self.runtime.supports_package_provisioning() || self.package_provisioner.is_some(),
        )
    }

    async fn probe_ready(&self) -> Result<(), pc::SandboxError> {
        self.probe_runtime_ready().await
    }

    async fn create(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<Box<dyn pc::Sandbox>, pc::SandboxError> {
        Ok(Box::new(self.create_container(spec).await?))
    }

    async fn adopt(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<Box<dyn pc::Sandbox>, pc::SandboxError> {
        Ok(Box::new(self.adopt_container(handle).await?))
    }
}

/// A realized Session-owned container environment. The opaque agent channel is
/// returned together with the exact exec process by [`ContainerEnvironment::spawn_agent_process`].
pub struct ContainerSandbox<R: ContainerRuntime> {
    runtime: Arc<R>,
    id: String,
    container_id: String,
    outputs_path: String,
    /// Secret-free base requirements retained across attempt processes.
    base_env: Vec<pc::EnvVar>,
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

    #[cfg(feature = "connection")]
    fn bind_scope(mut self, scope: impl Into<String>) -> Self {
        self.id = scope.into();
        self
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
    writeback_done: tokio::sync::Mutex<bool>,
    remove_done: tokio::sync::Mutex<bool>,
}

impl ContainerCleanupState {
    fn completed(secret_broker: Option<Arc<dyn pc::SecretBroker>>) -> Self {
        Self {
            staging: std::sync::Mutex::new(None),
            secret_writebacks: Vec::new(),
            secret_broker,
            memory: tokio::sync::Mutex::new(None),
            writeback_done: tokio::sync::Mutex::new(true),
            remove_done: tokio::sync::Mutex::new(false),
        }
    }

    async fn write_back_secrets<R: ContainerRuntime>(
        &self,
        runtime: &R,
        container_id: &str,
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
                    None => std::fs::read(&item.staged_path).map_err(|e| {
                        pc::SandboxError::new(format!("read refreshed credential file: {e}"))
                    })?,
                };
                broker.write_back(&item.reference, bytes).await?;
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
            if let Some(memory) = self.memory.lock().await.take() {
                for mount in memory {
                    mount.handle.teardown().await;
                }
            }
            self.staging.lock().expect("staging mutex poisoned").take();
            *done = true;
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
        pc::SandboxHandle::container(
            &self.id,
            pc::ContainerSandboxHandleV1 {
                container_id: self.container_id.clone(),
                outputs_path: self.outputs_path.clone(),
                base_env: self.base_env.clone(),
                live_input_projection: self.live_input_projection,
                continuation_excluded_paths: self.continuation_excluded_paths.clone(),
                runtime_handle: self.runtime_handle.clone(),
            },
        )
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

    async fn dispose(&self) -> Result<(), pc::SandboxError> {
        // Writable durable credentials belong to the Session environment and are
        // harvested exactly once when that environment terminates, not after each
        // attempt process.
        self.lifecycle
            .write_back_secrets(self.runtime.as_ref(), &self.container_id)
            .await?;
        // Native remote volumes are snapshots from the same canonical
        // MemoryMounter used by Docker/Podman. Copy the live Pod tree back into
        // that staging mount before teardown; the retained mount handle then
        // performs the existing CAS harvest. A read failure leaves the Pod and
        // handle live so disposal can be retried without claiming success.
        {
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
        self.lifecycle
            .dispose_once(
                self.runtime.as_ref(),
                &self.container_id,
                self.runtime_handle.as_ref(),
            )
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

/// Rootless Podman backend (daemonless CLI fork-exec). Gated behind the `podman`
/// feature; compile-verified here, running requires the `podman` binary.
#[cfg(feature = "podman")]
pub mod podman;

/// Warm container pool (pre-provisioned reusable capacity): cold-start + reuse. Gated
/// on `connection` (present under every real backend), which brings the tokio runtime
/// the pool's off-path replenish spawns onto.
#[cfg(feature = "connection")]
pub mod pool;
#[cfg(feature = "connection")]
pub use pool::WarmContainerPool;

#[cfg(test)]
mod tests;
