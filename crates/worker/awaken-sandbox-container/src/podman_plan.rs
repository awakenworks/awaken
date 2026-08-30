//! Pure rootfs and rootless-Podman planning for Session environments.

use awaken_provisioning_contract as pc;

use crate::{CgroupCaps, ContainerPlan, MANAGED_SANDBOX_LABEL, NetworkMode, writable_dirs};

/// The concrete rootfs a container/rootless-podman runtime realizes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum RootfsPlan {
    /// Borrow the default image's userland (no custom root).
    HostUserland,
    /// An OCI image reference.
    Image(String),
    /// A private root bound from a host directory template.
    RootDir {
        path_template: String,
        /// A writable base forces single-active use of the environment.
        writable: bool,
    },
    /// A private root unpacked from a tarball reference.
    RootTarball { reference: String, writable: bool },
}

/// Why a declared environment has no container-tier rootfs realization.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RootfsError {
    /// `Scope`/`LocalDir` are non-container tiers.
    #[error("environment kind has no container-tier rootfs realization")]
    NotAContainerRootfs,
}

/// Map a declared environment kind onto its container-tier rootfs, fail closed.
pub fn rootfs_plan(kind: &pc::EnvironmentKind) -> Result<RootfsPlan, RootfsError> {
    match kind {
        pc::EnvironmentKind::Sandbox => Ok(RootfsPlan::HostUserland),
        pc::EnvironmentKind::Image { reference } => Ok(RootfsPlan::Image(reference.clone())),
        pc::EnvironmentKind::IsolatedRoot {
            base,
            writable_base,
        } => Ok(match base {
            pc::RootfsSource::Dir { path_template } => RootfsPlan::RootDir {
                path_template: path_template.clone(),
                writable: *writable_base,
            },
            pc::RootfsSource::Tarball { reference } => RootfsPlan::RootTarball {
                reference: reference.clone(),
                writable: *writable_base,
            },
        }),
        pc::EnvironmentKind::Scope | pc::EnvironmentKind::LocalDir { .. } => {
            Err(RootfsError::NotAContainerRootfs)
        }
    }
}

fn declared_environment(spec: &pc::SandboxSpec) -> Option<pc::EnvironmentKind> {
    spec.environment.clone()
}

pub(crate) fn image_of(spec: &pc::SandboxSpec, default_image: &str) -> String {
    declared_environment(spec)
        .and_then(|environment| match environment {
            pc::EnvironmentKind::Image { reference } => Some(reference),
            _ => None,
        })
        .unwrap_or_else(|| default_image.to_owned())
}

/// Resolve the rootfs from the same canonical Environment that selects the
/// Docker/Kubernetes image. A non-container or absent declaration falls back to
/// the configured container image.
pub(crate) fn rootfs_of(spec: &pc::SandboxSpec, default_image: &str) -> RootfsPlan {
    declared_environment(spec)
        .and_then(|kind| rootfs_plan(&kind).ok())
        .unwrap_or_else(|| RootfsPlan::Image(image_of(spec, default_image)))
}

/// Render a deterministic rootless-Podman `run` argv for a Session environment.
#[must_use]
pub fn podman_run_argv(name: &str, plan: &ContainerPlan, rootfs: &RootfsPlan) -> Vec<String> {
    let mut argv: Vec<String> = ["run", "-d", "--init", "--name", name]
        .into_iter()
        .map(String::from)
        .collect();
    argv.extend(["--entrypoint".into(), String::new()]);
    argv.extend(["--label".into(), format!("{MANAGED_SANDBOX_LABEL}=1")]);
    argv.push("--read-only".into());
    for dir in writable_dirs(plan) {
        argv.extend([
            "--tmpfs".into(),
            format!("{dir}:rw,noexec,nosuid,mode=1777,size=64m"),
        ]);
    }
    match &plan.network {
        NetworkMode::Open => {}
        NetworkMode::None | NetworkMode::Allowlist => {
            argv.extend(["--network".into(), "none".into()]);
        }
    }
    let caps = CgroupCaps::from_limits(&plan.limits);
    if let Some(memory) = caps.memory_bytes {
        argv.extend(["--memory".into(), memory.to_string()]);
        argv.extend(["--memory-swap".into(), memory.to_string()]);
    }
    if let Some(cpu) = plan.limits.cpu_millis {
        argv.extend(["--cpus".into(), format!("{}.{:03}", cpu / 1000, cpu % 1000)]);
    }
    if let Some(pids) = caps.pids {
        argv.extend(["--pids-limit".into(), pids.to_string()]);
    }
    if let Some(size) = caps.disk_size {
        argv.extend(["--storage-opt".into(), format!("size={size}")]);
    }
    for (key, value) in &plan.env {
        argv.extend(["-e".into(), format!("{key}={value}")]);
    }
    for bind in &plan.binds {
        let read_only = if bind.read_only { ":ro" } else { "" };
        argv.extend([
            "-v".into(),
            format!("{}:{}{read_only}", bind.source_ref, bind.mount_path),
        ]);
    }
    match rootfs {
        RootfsPlan::HostUserland => argv.push(plan.image.clone()),
        RootfsPlan::Image(image) => argv.push(image.clone()),
        RootfsPlan::RootDir {
            path_template,
            writable,
        }
        | RootfsPlan::RootTarball {
            reference: path_template,
            writable,
        } => {
            let overlay = if *writable { "" } else { ":O" };
            argv.extend(["--rootfs".into(), format!("{path_template}{overlay}")]);
        }
    }
    argv.extend(plan.command.iter().cloned());
    argv
}
