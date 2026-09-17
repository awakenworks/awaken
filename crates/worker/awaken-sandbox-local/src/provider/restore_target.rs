//! Durable physical-target fencing for local restore composition.
//!
//! Checkpoint bytes and formats deliberately do not live here. The sole job of
//! this module is to reserve, recover, and dispose one exact provider directory
//! across `LocalProvider` instances and Host process loss.

#[cfg(test)]
mod tests;

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{LocalProvider, LocalSandbox, pc};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreTargetBinding {
    evidence: pc::SandboxRestorationEvidence,
}

fn restoration_target_id(
    evidence: &pc::SandboxRestorationEvidence,
) -> Result<String, pc::SandboxError> {
    Ok(format!("restore-{}", evidence.physical_target_key()?))
}

pub(super) fn restoration_root(
    base: &Path,
    evidence: &pc::SandboxRestorationEvidence,
) -> Result<PathBuf, pc::SandboxError> {
    Ok(crate::sandbox_dir(base, &restoration_target_id(evidence)?))
}

fn sidecar(root: &Path, suffix: &str) -> Result<PathBuf, pc::SandboxError> {
    let parent = root.parent().ok_or_else(|| {
        pc::SandboxError::invalid("local restoration root has no provider parent")
    })?;
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| pc::SandboxError::invalid("local restoration root is not portable UTF-8"))?;
    Ok(parent.join(format!(".{name}.awaken-restore-{suffix}")))
}

fn read_binding(root: &Path) -> Result<Option<RestoreTargetBinding>, pc::SandboxError> {
    match std::fs::read(sidecar(root, "target.json")?) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
            pc::SandboxError::invalid(format!("decode local restoration target evidence: {error}"))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(pc::SandboxError::backend(
            "read_binding",
            format!("read local restoration target evidence: {error}"),
        )),
    }
}

fn write_binding(
    root: &Path,
    evidence: &pc::SandboxRestorationEvidence,
) -> Result<(), pc::SandboxError> {
    let destination = sidecar(root, "target.json")?;
    let temporary = sidecar(root, &format!("target.{}.tmp", std::process::id()))?;
    let bytes = serde_json::to_vec(&RestoreTargetBinding {
        evidence: evidence.clone(),
    })
    .map_err(|error| {
        pc::SandboxError::backend(
            "write_binding",
            format!("encode local restoration target evidence: {error}"),
        )
    })?;
    match std::fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(pc::SandboxError::backend("write_binding", error)),
    }
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(|error| pc::SandboxError::backend("write_binding", error))?;
    file.write_all(&bytes)
        .map_err(|error| pc::SandboxError::backend("write_binding", error))?;
    file.sync_all()
        .map_err(|error| pc::SandboxError::backend("write_binding", error))?;
    std::fs::rename(&temporary, &destination)
        .map_err(|error| pc::SandboxError::backend("write_binding", error))?;
    std::fs::File::open(
        destination
            .parent()
            .ok_or_else(|| pc::SandboxError::invalid("local restoration evidence has no parent"))?,
    )
    .and_then(|directory| directory.sync_all())
    .map_err(|error| pc::SandboxError::backend("write_binding", error))
}

async fn lock_target(root: &Path) -> Result<std::fs::File, pc::SandboxError> {
    std::fs::create_dir_all(root.parent().ok_or_else(|| {
        pc::SandboxError::invalid("local restoration root has no provider parent")
    })?)
    .map_err(|error| pc::SandboxError::backend("lock_target", error))?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(sidecar(root, "lock")?)
        .map_err(|error| pc::SandboxError::backend("lock_target", error))?;
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(std::fs::TryLockError::WouldBlock) => {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(pc::SandboxError::backend("lock_target", error));
            }
        }
    }
}

/// Hold the same process-shared target lock across checkpoint materialization.
/// Acquisition and disposal use this inode too, so no concurrent caller can
/// observe, populate, or delete a split physical target.
pub(super) async fn lock_bound_target(
    root: &Path,
    expected: &pc::SandboxRestorationEvidence,
) -> Result<std::fs::File, pc::SandboxError> {
    let lock = lock_target(root).await?;
    verify_binding(root, expected)?;
    Ok(lock)
}

fn remove_target_binding(root: &Path) -> Result<(), pc::SandboxError> {
    match std::fs::remove_file(sidecar(root, "target.json")?) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(pc::SandboxError::backend("remove_target_binding", error)),
    }
    Ok(())
}

fn physical_directory_exists(root: &Path) -> Result<bool, pc::SandboxError> {
    match std::fs::symlink_metadata(root) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(pc::SandboxError::conflict(
            "local restoration target is not a physical provider directory",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(pc::SandboxError::backend(
            "physical_directory_exists",
            format!("inspect local restoration target directory: {error}"),
        )),
    }
}

pub(super) fn verify_binding(
    root: &Path,
    expected: &pc::SandboxRestorationEvidence,
) -> Result<(), pc::SandboxError> {
    match read_binding(root)? {
        Some(binding) if binding.evidence == *expected && physical_directory_exists(root)? => {
            Ok(())
        }
        Some(binding) if binding.evidence != *expected => Err(pc::SandboxError::conflict(
            "local restoration target belongs to a different exact effect",
        )),
        Some(_) => Err(pc::SandboxError::not_found(
            "local restoration evidence has no physical target directory",
        )),
        None => Err(pc::SandboxError::not_found(
            "local restoration target has no durable evidence",
        )),
    }
}

impl LocalProvider {
    pub(super) async fn acquire_restore_sandbox(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
    ) -> Result<pc::SandboxRestoreTarget<LocalSandbox>, pc::SandboxError> {
        request.validate_for_spec(spec)?;
        self.prepare(spec)?;
        let evidence = request.evidence(spec);
        let root = restoration_root(&self.base, &evidence)?;

        // The process-shared lock precedes the read-first observation. It closes
        // both first-acquisition and exact-disposal races without consulting any
        // checkpoint bytes or process-local slot.
        let _lock = lock_target(&root).await?;
        let disposition = match read_binding(&root)? {
            Some(binding) if binding.evidence == evidence => {
                if !physical_directory_exists(&root)? {
                    std::fs::create_dir(&root).map_err(|error| {
                        pc::SandboxError::backend("acquire_restore_sandbox", error)
                    })?;
                }
                pc::SandboxRestoreTargetDisposition::Recovered
            }
            Some(_) => {
                return Err(pc::SandboxError::conflict(
                    "local restoration target belongs to a different exact effect",
                ));
            }
            None if physical_directory_exists(&root)? => {
                return Err(pc::SandboxError::conflict(
                    "local restoration target directory exists without exact evidence",
                ));
            }
            None => {
                // Evidence is durable before the directory becomes observable;
                // a crash in this gap is repaired by the exact request only.
                write_binding(&root, &evidence)?;
                std::fs::create_dir(&root)
                    .map_err(|error| pc::SandboxError::backend("acquire_restore_sandbox", error))?;
                pc::SandboxRestoreTargetDisposition::Created
            }
        };
        self.restore_target_with_disposition(spec, request, root, disposition)
    }

    fn restore_target_with_disposition(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
        root: PathBuf,
        disposition: pc::SandboxRestoreTargetDisposition,
    ) -> Result<pc::SandboxRestoreTarget<LocalSandbox>, pc::SandboxError> {
        let handle = request.bind_handle(
            spec,
            pc::SandboxHandle::local(
                &spec.scope,
                pc::LocalSandboxHandleV1 {
                    outputs_path: spec.outputs_path.clone(),
                    base_env: spec.env.clone(),
                    continuation_excluded_paths: request.checkpoint.excluded_mounts.clone(),
                    deny_tool_egress: spec.deny_tool_egress,
                },
            ),
        )?;
        let mut sandbox =
            self.build_at(&spec.scope, &spec.outputs_path, root, Some(handle.clone()));
        sandbox.base_env.clone_from(&spec.env);
        sandbox.deny_egress = spec.deny_tool_egress;
        sandbox.continuation_excluded_paths = request
            .checkpoint
            .excluded_mounts
            .iter()
            .map(|path| sandbox.root.resolve(path).map_err(pc::SandboxError::from))
            .collect::<Result<_, _>>()?;
        std::fs::create_dir_all(
            sandbox
                .root
                .resolve(&spec.outputs_path)
                .map_err(pc::SandboxError::from)?,
        )
        .map_err(|error| pc::SandboxError::backend("restore_target_with_disposition", error))?;
        pc::SandboxRestoreTarget::exact(request, spec, sandbox, &handle, disposition)
    }

    pub(super) async fn dispose_restore_sandbox(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
        authorization: &pc::SandboxDisposalAuthorization,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError> {
        authorization.validate()?;
        pc::with_current_sandbox_effect(authorization.effect_fence(), authority, || Ok(()))?;
        request.validate_for_spec(spec)?;
        let evidence = request.evidence(spec);
        let root = restoration_root(&self.base, &evidence)?;
        dispose_bound_target(&root, &evidence, authorization.effect_fence(), authority).await
    }
}

pub(super) async fn dispose_bound_target(
    root: &Path,
    expected: &pc::SandboxRestorationEvidence,
    fence: &pc::SandboxEffectFence,
    authority: &dyn pc::CurrentExecutionAuthority,
) -> Result<(), pc::SandboxError> {
    pc::with_current_sandbox_effect(fence, authority, || Ok(()))?;
    let _lock = lock_target(root).await?;
    // The exact target lock and current owner guard jointly cover the whole
    // synchronous deletion. Waiting for the former cannot preserve stale
    // authority, and even exact absence is not an unfenced success receipt.
    pc::with_current_sandbox_effect(fence, authority, || {
        match read_binding(root)? {
            Some(binding) if binding.evidence != *expected => {
                return Err(pc::SandboxError::conflict(
                    "local restored-target cleanup observed another exact effect",
                ));
            }
            None if physical_directory_exists(root)? => {
                return Err(pc::SandboxError::conflict(
                    "local restored-target cleanup found an unfenced directory",
                ));
            }
            Some(_) if physical_directory_exists(root)? => {
                let identity = awaken_sandbox_fs::directory_identity_nofollow(root)
                    .map_err(|error| pc::SandboxError::backend("dispose_bound_target", error))?;
                awaken_sandbox_fs::remove_directory_tree_exact(root, identity)
                    .map_err(|error| pc::SandboxError::backend("dispose_bound_target", error))?;
            }
            Some(_) | None => {}
        }
        // Keep the exact lock inode as the process-shared synchronization point.
        // Removing it while waiters still hold/open it would split one target into
        // two independent locks. It contains no environment state or evidence.
        remove_target_binding(root)
    })
}
