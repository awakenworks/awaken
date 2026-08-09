//! Provider-neutral admission and resolution for the one live Managed File tree.

use super::*;

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
