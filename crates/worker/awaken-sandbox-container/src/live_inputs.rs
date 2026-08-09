//! Provider-neutral admission and resolution for the one live Managed File tree.

use super::*;

static HOST_PROJECTION_SEQUENCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

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

/// One classification for initial planning and subsequent attach/remove. Keeping
/// it here prevents Docker/Podman and Kubernetes from drifting on which mounts
/// belong to the Managed File projection.
pub(crate) fn manages(bind: &BindPlan) -> bool {
    bind.read_only && live_input_relative_path(&bind.mount_path).is_some()
}

fn host_projection_path(
    root: &std::path::Path,
    path: &str,
) -> Result<std::path::PathBuf, RuntimeError> {
    let relative = live_input_relative_path(path).ok_or_else(|| {
        RuntimeError::Backend("live input path escaped its projection root".into())
    })?;
    Ok(root.join(relative))
}

fn set_projected_file_permissions(path: &std::path::Path) -> Result<(), RuntimeError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o444))
            .map_err(|error| RuntimeError::Backend(error.to_string()))?;
    }
    Ok(())
}

/// Atomically replace one file below a trusted Host projection root. The root is
/// bind-mounted read-only into Docker/Podman, so only the runtime process can
/// mutate membership while the Agent observes generation changes in place.
pub(crate) fn project_host_input(
    root: &std::path::Path,
    path: &str,
    bytes: &[u8],
) -> Result<(), RuntimeError> {
    use std::sync::atomic::Ordering;

    let target = host_projection_path(root, path)?;
    let parent = target
        .parent()
        .ok_or_else(|| RuntimeError::Backend("live input has no projection parent".into()))?;
    std::fs::create_dir_all(parent).map_err(|error| RuntimeError::Backend(error.to_string()))?;
    let temp = parent.join(format!(
        ".awaken-live-input-{}-{}.tmp",
        std::process::id(),
        HOST_PROJECTION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        std::fs::write(&temp, bytes).map_err(|error| RuntimeError::Backend(error.to_string()))?;
        set_projected_file_permissions(&temp)?;
        std::fs::rename(&temp, &target).map_err(|error| RuntimeError::Backend(error.to_string()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Idempotently remove one projected file and empty descendant directories,
/// stopping at the stable bind root.
#[cfg(any(feature = "docker", feature = "podman", test))]
pub(crate) fn remove_host_input(root: &std::path::Path, path: &str) -> Result<(), RuntimeError> {
    let target = host_projection_path(root, path)?;
    match std::fs::remove_file(&target) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(RuntimeError::Backend(error.to_string())),
    }
    let mut parent = target.parent();
    while let Some(directory) = parent {
        if directory == root || !directory.starts_with(root) {
            break;
        }
        if std::fs::remove_dir(directory).is_err() {
            break;
        }
        parent = directory.parent();
    }
    Ok(())
}

/// Collapse all initial Managed Files into one stable Host directory bind. This
/// is the Docker/Podman counterpart of the Kubernetes projector volume: the
/// environment identity remains stable when later generations add or remove files.
pub(super) fn stage_host_projection(
    scope: &str,
    binds: &mut Vec<BindPlan>,
    guard: &mut Option<StagingGuard>,
) -> Result<(), pc::SandboxError> {
    let root = staging_dir(guard, scope)?.join("live-inputs");
    std::fs::create_dir_all(&root)
        .map_err(|error| err(RuntimeError::Backend(error.to_string())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| err(RuntimeError::Backend(error.to_string())))?;
    }

    let mut retained = Vec::with_capacity(binds.len() + 1);
    for bind in std::mem::take(binds) {
        if manages(&bind) {
            let bytes = std::fs::read(&bind.source_ref)
                .map_err(|error| err(RuntimeError::Backend(error.to_string())))?;
            project_host_input(&root, &bind.mount_path, &bytes).map_err(err)?;
        } else {
            retained.push(bind);
        }
    }
    retained.push(BindPlan {
        source_ref: root.to_string_lossy().into_owned(),
        mount_path: LIVE_INPUTS_ROOT.into(),
        read_only: true,
        content: None,
        content_bytes: None,
        secret_content: None,
        secret_writeback: false,
        credential_file_path: None,
    });
    *binds = retained;
    Ok(())
}

impl<R: ContainerRuntime + 'static> ContainerSandbox<R> {
    fn validates_live_input(requirement: &pc::MountRequirement) -> bool {
        live_input_relative_path(&requirement.mount_path).is_some()
            && requirement.access == pc::MountAccess::ReadOnly
            && matches!(
                requirement.source,
                pc::MountSource::File { .. }
                    | pc::MountSource::Resource { .. }
                    | pc::MountSource::Inline { .. }
                    | pc::MountSource::InlineBytes { .. }
            )
    }

    pub fn supports_live_mount_replacement(
        &self,
        previous: &[pc::MountRequirement],
        next: &[pc::MountRequirement],
    ) -> bool {
        if !self.live_input_projection {
            return false;
        }
        previous
            .iter()
            .chain(next)
            .filter(|mount| !(previous.contains(*mount) && next.contains(*mount)))
            .all(Self::validates_live_input)
    }

    async fn resolve_live_input(
        &self,
        requirement: &pc::MountRequirement,
    ) -> Result<Vec<u8>, pc::SandboxError> {
        let bytes = match &requirement.source {
            pc::MountSource::Inline { contents } => Some(contents.as_bytes().to_vec()),
            pc::MountSource::InlineBytes { contents, .. } => Some(contents.clone()),
            pc::MountSource::File { .. } | pc::MountSource::Resource { .. } => {
                resolve_blob(
                    &requirement.source,
                    &self.blobs,
                    &self.file_store,
                    &self.lifecycle.secret_broker,
                )
                .await?
            }
            _ => None,
        }
        .ok_or_else(|| {
            err(RuntimeError::Backend(format!(
                "live input `{}` did not resolve to bytes",
                requirement.mount_path
            )))
        })?;
        verify_hash(&requirement.source, &bytes)?;
        Ok(bytes)
    }

    pub async fn remove_live_input_path(&self, path: &str) -> Result<(), pc::SandboxError> {
        if live_input_relative_path(path).is_none() || !self.live_input_projection {
            return Err(err(RuntimeError::Backend(
                "late mount removal is unsupported on this container tier".into(),
            )));
        }
        self.runtime
            .remove_live_input(&self.container_id, path)
            .await
            .map_err(err)
    }

    pub(super) async fn attach_live_input(
        &self,
        req: pc::MountRequirement,
    ) -> Result<pc::RealizedMount, pc::SandboxError> {
        if !self.live_input_projection || !Self::validates_live_input(&req) {
            return Err(err(RuntimeError::Backend(
                "late attach unsupported on this container tier or outside its read-only input root"
                    .into(),
            )));
        }
        let bytes = self.resolve_live_input(&req).await?;
        self.runtime
            .project_live_input(&self.container_id, &req.mount_path, &bytes)
            .await
            .map_err(err)?;
        Ok(pc::RealizedMount {
            mount_id: req.mount_id,
            mount_path: req.mount_path,
            access: req.access,
            realization: pc::Realization::Bind,
            content_hash: declared_hash(&req.source).map(str::to_owned),
        })
    }
}
