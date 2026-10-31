// @formal surface obligation=session_resources.install_replacement_requires_newer_or_unattempted_authority_amendment
// @formal surface obligation=worker.secret_materialization_is_claim_fenced
// @formal reason: Installs claim-scoped live Session inputs.
//! Provider-neutral admission and resolution for the one live Managed File tree.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;
use awaken_container_contract::RuntimeError;

static HOST_PROJECTION_SEQUENCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

// The projection root and its one lexical admission rule are shared with the
// Session owner through the container contract.
use awaken_container_contract::{LIVE_INPUTS_ROOT, live_input_relative_path};

/// One classification for initial planning and subsequent attach/remove. Keeping
/// it here prevents Docker/Podman and Kubernetes from drifting on which mounts
/// belong to the Managed File projection.
pub fn manages_live_input(bind: &BindPlan) -> bool {
    bind.read_only && live_input_relative_path(&bind.mount_path).is_some()
}

fn host_projection_path(
    root: &std::path::Path,
    path: &str,
) -> Result<std::path::PathBuf, RuntimeError> {
    let relative = live_input_relative_path(path).ok_or_else(|| {
        RuntimeError::Backend("live input path escaped its projection root".into())
    })?;
    if std::path::Path::new(relative)
        .components()
        .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Err(RuntimeError::Backend(
            "live input path escaped its projection root".into(),
        ));
    }
    Ok(root.join(relative))
}

fn set_projected_permissions(path: &std::path::Path, directory: bool) -> std::io::Result<()> {
    #[cfg(not(unix))]
    let _ = (path, directory);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if directory { 0o755 } else { 0o444 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

fn ensure_projected_directory(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "live input projection parent is a symlink or not a directory",
        ));
    }
    // The Agent's bind stays read-only; the Host needs owner-write access for
    // later atomic generations. Never normalize the private staging ancestors.
    set_projected_permissions(path, true)
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
    let relative_parent = parent
        .strip_prefix(root)
        .map_err(|_| RuntimeError::Backend("live input path escaped its projection root".into()))?;
    ensure_projected_directory(root).map_err(|error| RuntimeError::Backend(error.to_string()))?;
    let mut directory = root.to_path_buf();
    for part in relative_parent.components() {
        directory.push(part);
        ensure_projected_directory(&directory)
            .map_err(|error| RuntimeError::Backend(error.to_string()))?;
    }
    let temp = parent.join(format!(
        ".awaken-live-input-{}-{}.tmp",
        std::process::id(),
        HOST_PROJECTION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        std::fs::write(&temp, bytes).map_err(|error| RuntimeError::Backend(error.to_string()))?;
        set_projected_permissions(&temp, false)
            .map_err(|error| RuntimeError::Backend(error.to_string()))?;
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
    ensure_projected_directory(&root)
        .map_err(|error| pc::SandboxError::backend("stage_host_projection", error.to_string()))?;

    let mut retained = Vec::with_capacity(binds.len() + 1);
    for bind in std::mem::take(binds) {
        if manages_live_input(&bind) {
            let bytes = std::fs::read(&bind.source_ref).map_err(|error| {
                pc::SandboxError::backend("stage_host_projection", error.to_string())
            })?;
            project_host_input(&root, &bind.mount_path, &bytes).map_err(pc::SandboxError::from)?;
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
            pc::SandboxError::invalid(format!(
                "live input `{}` did not resolve to bytes",
                requirement.mount_path
            ))
        })?;
        verify_hash(&requirement.source, &bytes)?;
        Ok(bytes)
    }

    pub async fn remove_live_input_path(&self, path: &str) -> Result<(), pc::SandboxError> {
        if live_input_relative_path(path).is_none() || !self.live_input_projection {
            return Err(pc::SandboxError::unsupported(
                "late mount removal is unsupported on this container tier",
            ));
        }
        self.runtime
            .remove_live_input(&self.container_id, path)
            .await
            .map_err(pc::SandboxError::from)
    }

    pub(super) async fn attach_live_input(
        &self,
        req: pc::MountRequirement,
    ) -> Result<pc::RealizedMount, pc::SandboxError> {
        if !self.live_input_projection || !Self::validates_live_input(&req) {
            return Err(pc::SandboxError::unsupported(
                "late attach unsupported on this container tier or outside its read-only input root",
            ));
        }
        let bytes = self.resolve_live_input(&req).await?;
        self.runtime
            .project_live_input(&self.container_id, &req.mount_path, &bytes)
            .await
            .map_err(pc::SandboxError::from)?;
        Ok(pc::RealizedMount {
            mount_id: req.mount_id,
            mount_path: req.mount_path,
            access: req.access,
            realization: pc::Realization::Bind,
            content_hash: declared_hash(&req.source).map(str::to_owned),
        })
    }
}
