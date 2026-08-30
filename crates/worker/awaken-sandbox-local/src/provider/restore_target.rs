//! Durable physical-target fencing for local restore composition.
//!
//! Checkpoint bytes and formats deliberately do not live here. The sole job of
//! this module is to reserve, recover, and dispose one exact provider directory
//! across `LocalProvider` instances and Host process loss.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{LocalProvider, LocalSandbox, err, pc};

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
    let parent = root
        .parent()
        .ok_or_else(|| err("local restoration root has no provider parent"))?;
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| err("local restoration root is not portable UTF-8"))?;
    Ok(parent.join(format!(".{name}.awaken-restore-{suffix}")))
}

fn read_binding(root: &Path) -> Result<Option<RestoreTargetBinding>, pc::SandboxError> {
    match std::fs::read(sidecar(root, "target.json")?) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| err(format!("decode local restoration target evidence: {error}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(err(format!(
            "read local restoration target evidence: {error}"
        ))),
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
    .map_err(|error| err(format!("encode local restoration target evidence: {error}")))?;
    match std::fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(err(error)),
    }
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(err)?;
    file.write_all(&bytes).map_err(err)?;
    file.sync_all().map_err(err)?;
    std::fs::rename(&temporary, &destination).map_err(err)?;
    std::fs::File::open(
        destination
            .parent()
            .ok_or_else(|| err("local restoration evidence has no parent"))?,
    )
    .and_then(|directory| directory.sync_all())
    .map_err(err)
}

async fn lock_target(root: &Path) -> Result<std::fs::File, pc::SandboxError> {
    std::fs::create_dir_all(
        root.parent()
            .ok_or_else(|| err("local restoration root has no provider parent"))?,
    )
    .map_err(err)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(sidecar(root, "lock")?)
        .map_err(err)?;
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(std::fs::TryLockError::WouldBlock) => {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(err(error)),
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
        Err(error) => return Err(err(error)),
    }
    Ok(())
}

fn physical_directory_exists(root: &Path) -> Result<bool, pc::SandboxError> {
    match std::fs::symlink_metadata(root) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(err(
            "local restoration target is not a physical provider directory",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(err(format!(
            "inspect local restoration target directory: {error}"
        ))),
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
        Some(binding) if binding.evidence != *expected => Err(err(
            "local restoration target belongs to a different exact effect",
        )),
        Some(_) => Err(err(
            "local restoration evidence has no physical target directory",
        )),
        None => Err(err("local restoration target has no durable evidence")),
    }
}

impl LocalProvider {
    pub(super) async fn acquire_restore_sandbox(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
    ) -> Result<pc::SandboxRestoreTarget<LocalSandbox>, pc::SandboxError> {
        request.validate_for_spec(spec)?;
        pc::prepare_environment(spec, &LocalProvider::capabilities()).map_err(err)?;
        let evidence = request.evidence(spec);
        let root = restoration_root(&self.base, &evidence)?;

        // The process-shared lock precedes the read-first observation. It closes
        // both first-acquisition and exact-disposal races without consulting any
        // checkpoint bytes or process-local slot.
        let _lock = lock_target(&root).await?;
        let disposition = match read_binding(&root)? {
            Some(binding) if binding.evidence == evidence => {
                if !physical_directory_exists(&root)? {
                    std::fs::create_dir(&root).map_err(err)?;
                }
                pc::SandboxRestoreTargetDisposition::Recovered
            }
            Some(_) => {
                return Err(err(
                    "local restoration target belongs to a different exact effect",
                ));
            }
            None if physical_directory_exists(&root)? => {
                return Err(err(
                    "local restoration target directory exists without exact evidence",
                ));
            }
            None => {
                // Evidence is durable before the directory becomes observable;
                // a crash in this gap is repaired by the exact request only.
                write_binding(&root, &evidence)?;
                std::fs::create_dir(&root).map_err(err)?;
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
            .map(|path| sandbox.root.resolve(path).map_err(err))
            .collect::<Result<_, _>>()?;
        std::fs::create_dir_all(sandbox.root.resolve(&spec.outputs_path).map_err(err)?)
            .map_err(err)?;
        pc::SandboxRestoreTarget::exact(request, spec, sandbox, &handle, disposition)
    }

    pub(super) async fn dispose_restore_sandbox(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
    ) -> Result<(), pc::SandboxError> {
        request.validate_for_spec(spec)?;
        let evidence = request.evidence(spec);
        let root = restoration_root(&self.base, &evidence)?;
        dispose_bound_target(&root, &evidence).await
    }
}

pub(super) async fn dispose_bound_target(
    root: &Path,
    expected: &pc::SandboxRestorationEvidence,
) -> Result<(), pc::SandboxError> {
    let _lock = lock_target(root).await?;
    match read_binding(root)? {
        Some(binding) if binding.evidence != *expected => {
            return Err(err(
                "local restored-target cleanup observed another exact effect",
            ));
        }
        None if physical_directory_exists(root)? => {
            return Err(err(
                "local restored-target cleanup found an unfenced directory",
            ));
        }
        Some(_) if physical_directory_exists(root)? => {
            let identity = awaken_sandbox_fs::directory_identity_nofollow(root).map_err(err)?;
            awaken_sandbox_fs::remove_directory_tree_exact(root, identity).map_err(err)?;
        }
        Some(_) | None => {}
    }
    // Keep the exact lock inode as the process-shared synchronization point.
    // Removing it while waiters still hold/open it would split one target into
    // two independent locks. It contains no environment state or evidence.
    remove_target_binding(root)
}
