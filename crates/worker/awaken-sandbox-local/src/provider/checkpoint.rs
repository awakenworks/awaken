//! Portable filesystem checkpoint encoding for the local provider.

use std::io::Write;
use std::path::Path;

use super::{LocalProvider, LocalSandbox, content_fingerprint, err, pc};

fn append_tree<W: Write>(
    archive: &mut tar::Builder<W>,
    root: &Path,
    relative: &Path,
    excluded: &[std::path::PathBuf],
) -> Result<(), pc::SandboxError> {
    let current = root.join(relative);
    if excluded
        .iter()
        .any(|path| current == *path || current.starts_with(path))
    {
        return Ok(());
    }
    let metadata = std::fs::symlink_metadata(&current).map_err(err)?;
    if !relative.as_os_str().is_empty() {
        if metadata.is_dir() {
            archive.append_dir(relative, &current).map_err(err)?;
        } else {
            archive
                .append_path_with_name(&current, relative)
                .map_err(err)?;
            return Ok(());
        }
    }
    for entry in std::fs::read_dir(&current).map_err(err)? {
        let entry = entry.map_err(err)?;
        append_tree(archive, root, &relative.join(entry.file_name()), excluded)?;
    }
    Ok(())
}

impl LocalProvider {
    pub async fn restore_sandbox(
        &self,
        spec: &pc::SandboxSpec,
        checkpoint: &awaken_session_contract::SandboxCheckpointRef,
        store: &dyn pc::SandboxCheckpointStore,
    ) -> Result<LocalSandbox, pc::SandboxError> {
        if checkpoint.format != "awaken-fs-tar-v1" {
            return Err(err(format!(
                "unsupported checkpoint format {:?}",
                checkpoint.format
            )));
        }
        let bytes = store.get(&checkpoint.id).await?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != checkpoint.size_bytes
            || content_fingerprint(&bytes) != checkpoint.digest
        {
            return Err(err("checkpoint size or digest mismatch"));
        }
        let sandbox = self.create_sandbox(spec).await?;
        let root = sandbox.root.root().to_path_buf();
        if let Err(error) = tar::Archive::new(std::io::Cursor::new(bytes)).unpack(&root) {
            pc::Sandbox::dispose(&sandbox).await?;
            return Err(err(format!("restore checkpoint: {error}")));
        }
        Ok(sandbox)
    }
}

impl LocalSandbox {
    pub(super) async fn create_checkpoint(
        &self,
        request: &pc::SandboxCheckpointRequest,
        store: &dyn pc::SandboxCheckpointStore,
    ) -> Result<awaken_session_contract::CheckpointReceipt, pc::SandboxError> {
        if request.format != "awaken-fs-tar-v1" {
            return Err(err(format!(
                "unsupported checkpoint format {:?}",
                request.format
            )));
        }
        let mut bytes = Vec::new();
        {
            let mut archive = tar::Builder::new(&mut bytes);
            append_tree(
                &mut archive,
                self.root.root(),
                Path::new(""),
                &self.continuation_excluded_paths,
            )?;
            archive.finish().map_err(err)?;
        }
        let size_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if size_bytes > request.max_bytes {
            return Err(err(format!(
                "checkpoint is {size_bytes} bytes, limit is {}",
                request.max_bytes
            )));
        }
        let digest = content_fingerprint(&bytes);
        let stored = store
            .put(
                &pc::CheckpointObjectMetadata {
                    session_id: request.session_id.clone(),
                    generation_id: request.generation.id.clone(),
                    suspend_effect_id: request.operation.effect_id.clone(),
                    expires_at_unix_ms: request.expires_at_unix_ms,
                },
                bytes,
            )
            .await?;
        if stored.digest != digest || stored.size_bytes != size_bytes {
            return Err(err("checkpoint store durability receipt mismatch"));
        }
        let checkpoint = awaken_session_contract::SandboxCheckpointRef {
            id: stored.id,
            format: request.format.clone(),
            digest,
            size_bytes,
            created_at_unix_ms: request.created_at_unix_ms,
            expires_at_unix_ms: request.expires_at_unix_ms,
            environment_fingerprint: request.generation.environment_fingerprint.clone(),
            base_image_fingerprint: request.generation.base_image_fingerprint.clone(),
            excluded_mounts: self
                .continuation_excluded_paths
                .iter()
                .filter_map(|path| path.strip_prefix(self.root.root()).ok())
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            suspend_effect_id: request.operation.effect_id.clone(),
        };
        Ok(awaken_session_contract::CheckpointReceipt {
            effect_id: request.operation.effect_id.clone(),
            generation_id: request.generation.id.clone(),
            checkpoint,
        })
    }
}
