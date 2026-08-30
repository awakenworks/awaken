//! Portable filesystem checkpoint encoding for the local provider.

use std::io::{Read as _, Write};
use std::path::Path;

use super::{LocalProvider, LocalSandbox, content_fingerprint, err, pc};

fn append_snapshot<W: Write>(
    archive: &mut tar::Builder<W>,
    snapshot: &awaken_sandbox_fs::TreeSnapshot,
) -> Result<(), pc::SandboxError> {
    for directory in &snapshot.directories {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(directory.mode);
        header.set_size(0);
        header.set_cksum();
        archive
            .append_data(&mut header, &directory.relative_path, std::io::empty())
            .map_err(err)?;
    }
    for file in &snapshot.files {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(file.mode);
        header.set_size(u64::try_from(file.bytes.len()).unwrap_or(u64::MAX));
        header.set_cksum();
        archive
            .append_data(&mut header, &file.relative_path, file.bytes.as_slice())
            .map_err(err)?;
    }
    Ok(())
}

fn validate_checkpoint_effect(
    request: &pc::SandboxCheckpointRequest,
    effect_fence: &pc::SandboxEffectFence,
) -> Result<(), pc::SandboxError> {
    if request.effect_id == effect_fence.operation_id {
        Ok(())
    } else {
        Err(err(
            "checkpoint request effect id does not match its Sandbox effect fence",
        ))
    }
}

fn checkpoint_request_fingerprint(
    request: &pc::SandboxCheckpointRequest,
    effect_fence: &pc::SandboxEffectFence,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"awaken-filesystem-checkpoint-request/v1\0");
    for value in [
        request.workspace_id.as_str(),
        request.session_id.as_str(),
        request.generation_id.as_str(),
        request.environment_fingerprint.as_str(),
        request.base_image_fingerprint.as_str(),
        request.effect_id.as_str(),
        request.format.as_str(),
        effect_fence.operation_id.as_str(),
        effect_fence.owner.as_str(),
        effect_fence.runtime_incarnation.as_str(),
    ] {
        hasher.update(&(value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    for value in [
        request.created_at_unix_ms,
        request.expires_at_unix_ms,
        request.max_bytes,
        effect_fence.epoch,
    ] {
        hasher.update(&value.to_be_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn validate_checkpoint_reference(
    request: &pc::SandboxCheckpointRequest,
    reference: &pc::SandboxCheckpointRef,
) -> Result<(), pc::SandboxError> {
    if reference.format != request.format
        || reference.created_at_unix_ms != request.created_at_unix_ms
        || reference.expires_at_unix_ms != request.expires_at_unix_ms
        || reference.environment_fingerprint != request.environment_fingerprint
        || reference.base_image_fingerprint != request.base_image_fingerprint
        || reference.suspend_effect_id != request.effect_id
        || reference.size_bytes > request.max_bytes
    {
        Err(err(
            "recorded filesystem checkpoint does not match its exact request metadata",
        ))
    } else {
        Ok(())
    }
}

impl LocalProvider {
    /// Materialize one exact aggregate-projected restore target. Physical target
    /// acquisition, replay, and disposal share the provider-owned binding lock;
    /// the checkpoint store remains byte custody only.
    pub(super) async fn restore_exact_sandbox(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
        store: &dyn pc::SandboxCheckpointStore,
    ) -> Result<pc::SandboxRestoreResult<LocalSandbox>, pc::SandboxError> {
        request.validate_for_spec(spec)?;
        if request.checkpoint.format != "awaken-fs-tar-v1" {
            return Err(err(format!(
                "unsupported checkpoint format {:?}",
                request.checkpoint.format
            )));
        }
        let target = self.acquire_restore_sandbox(spec, request).await?;
        let evidence = target.evidence().clone();
        let sandbox = target.into_target();
        let bytes = store.get(&request.checkpoint.id).await?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != request.checkpoint.size_bytes
            || content_fingerprint(&bytes) != request.checkpoint.digest
        {
            return Err(err("checkpoint size or digest mismatch"));
        }

        let _target_lock =
            super::restore_target::lock_bound_target(sandbox.root.root(), &evidence).await?;
        let root_identity =
            awaken_sandbox_fs::directory_identity_nofollow(sandbox.root.root()).map_err(err)?;
        extract_checkpoint(&sandbox, root_identity, &bytes, || {
            super::restore_target::verify_binding(sandbox.root.root(), &evidence)
        })?;
        let handle = pc::Sandbox::handle(&sandbox);
        pc::SandboxRestoreResult::complete(request, spec, sandbox, &handle)
    }
}

pub(super) fn extract_checkpoint(
    sandbox: &LocalSandbox,
    root_identity: awaken_sandbox_fs::DirectoryIdentity,
    bytes: &[u8],
    mut validate_before_mutation: impl FnMut() -> Result<(), pc::SandboxError>,
) -> Result<(), pc::SandboxError> {
    let root = sandbox.root.root();
    let mut archive = tar::Archive::new(std::io::Cursor::new(bytes));
    let mut seen = std::collections::BTreeSet::new();
    let mut directory_modes = Vec::new();
    for entry in archive.entries().map_err(err)? {
        let mut entry = entry.map_err(err)?;
        let path = entry.path().map_err(err)?.into_owned();
        if path.as_os_str().is_empty()
            || path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(err(format!(
                "checkpoint entry `{}` is not one canonical relative path",
                path.display()
            )));
        }
        if !seen.insert(path.clone()) {
            return Err(err(format!(
                "checkpoint entry `{}` is duplicated",
                path.display()
            )));
        }
        let destination = root.join(&path);
        if sandbox
            .continuation_excluded_paths
            .iter()
            .any(|excluded| destination == *excluded || destination.starts_with(excluded))
        {
            return Err(err(format!(
                "checkpoint entry `{}` overlaps an independently governed mount",
                path.display()
            )));
        }
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            let mode = entry.header().mode().map_err(err)? & 0o777;
            validate_before_mutation()?;
            awaken_sandbox_fs::create_relative_directory_all(root, root_identity, &path)
                .map_err(err)?;
            directory_modes.push((path, mode));
        } else if entry_type.is_file() {
            let mut contents = Vec::new();
            entry.read_to_end(&mut contents).map_err(err)?;
            let mode = entry.header().mode().map_err(err)? & 0o777;
            validate_before_mutation()?;
            awaken_sandbox_fs::write_relative_file_atomic(
                root,
                root_identity,
                &path,
                &contents,
                mode,
            )
            .map_err(err)?;
        } else {
            return Err(err(format!(
                "checkpoint entry `{}` is not a regular file or directory",
                path.display()
            )));
        }
    }
    // Apply potentially read-only directory modes only after every child has
    // been written. Deepest-first prevents a parent mode from blocking the
    // nofollow walk to a child's final metadata mutation.
    directory_modes.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
    for (path, mode) in directory_modes {
        validate_before_mutation()?;
        awaken_sandbox_fs::set_relative_directory_mode(root, root_identity, &path, mode)
            .map_err(err)?;
    }
    Ok(())
}

impl LocalSandbox {
    fn checkpoint_snapshot(&self) -> Result<Vec<u8>, pc::SandboxError> {
        let root_identity = self.require_root_identity()?;
        let excluded = self
            .continuation_excluded_paths
            .iter()
            .map(|path| {
                let relative = path.strip_prefix(self.root.root()).map_err(|_| {
                    err(format!(
                        "checkpoint exclusion `{}` is outside sandbox root `{}`",
                        path.display(),
                        self.root.root().display()
                    ))
                })?;
                if relative.as_os_str().is_empty() {
                    return Err(err("checkpoint cannot exclude the complete sandbox root"));
                }
                Ok(relative.to_owned())
            })
            .collect::<Result<Vec<_>, pc::SandboxError>>()?;
        let snapshot = awaken_sandbox_fs::read_tree_nofollow_excluding(
            self.root.root(),
            root_identity,
            Path::new(""),
            &excluded,
        )
        .map_err(err)?;

        let mut bytes = Vec::new();
        {
            let mut archive = tar::Builder::new(&mut bytes);
            append_snapshot(&mut archive, &snapshot)?;
            archive.finish().map_err(err)?;
        }
        Ok(bytes)
    }

    async fn create_checkpoint_inner(
        &self,
        request: &pc::SandboxCheckpointRequest,
        store: &dyn pc::SandboxCheckpointStore,
        mut operation: Option<(
            &mut dyn crate::realization_marker::CheckpointParticipantGuard,
            &str,
        )>,
    ) -> Result<pc::SandboxCheckpointRef, pc::SandboxError> {
        if request.format != "awaken-fs-tar-v1" {
            return Err(err(format!(
                "unsupported checkpoint format {:?}",
                request.format
            )));
        }
        let bytes = self.checkpoint_snapshot()?;
        let size_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if size_bytes > request.max_bytes {
            return Err(err(format!(
                "checkpoint is {size_bytes} bytes, limit is {}",
                request.max_bytes
            )));
        }
        let digest = content_fingerprint(&bytes);
        if let Some((operation, request_fingerprint)) = operation.as_mut() {
            if let Some(reference) =
                (*operation).begin_checkpoint_upload(request_fingerprint, &digest, size_bytes)?
            {
                validate_checkpoint_reference(request, &reference)?;
                return Ok(reference);
            }
            (*operation).validate_before_effect()?;
        }
        let stored = store
            .put(
                &pc::CheckpointObjectMetadata {
                    workspace_id: request.workspace_id.clone(),
                    session_id: request.session_id.clone(),
                    generation_id: request.generation_id.clone(),
                    suspend_effect_id: request.effect_id.clone(),
                    created_at_unix_ms: request.created_at_unix_ms,
                    expires_at_unix_ms: request.expires_at_unix_ms,
                },
                bytes,
            )
            .await?;
        if stored.id.trim().is_empty() || stored.digest != digest || stored.size_bytes != size_bytes
        {
            return Err(err("checkpoint store durability receipt mismatch"));
        }
        let reference = pc::SandboxCheckpointRef {
            id: stored.id,
            format: request.format.clone(),
            digest,
            size_bytes,
            created_at_unix_ms: request.created_at_unix_ms,
            expires_at_unix_ms: request.expires_at_unix_ms,
            environment_fingerprint: request.environment_fingerprint.clone(),
            base_image_fingerprint: request.base_image_fingerprint.clone(),
            excluded_mounts: self
                .continuation_excluded_paths
                .iter()
                .filter_map(|path| path.strip_prefix(self.root.root()).ok())
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            suspend_effect_id: request.effect_id.clone(),
        };
        if let Some((operation, request_fingerprint)) = operation {
            operation.complete_checkpoint_upload(request_fingerprint, &reference)?;
        }
        Ok(reference)
    }

    async fn cleanup_checkpoint_with_participant(
        &self,
        request: &pc::SandboxCheckpointRequest,
        store: &dyn pc::SandboxCheckpointStore,
        request_fingerprint: &str,
        participant: &mut dyn crate::realization_marker::CheckpointParticipantGuard,
    ) -> Result<(), pc::SandboxError> {
        let reference = if let Some(reference) =
            participant.completed_checkpoint(request_fingerprint)?
        {
            validate_checkpoint_reference(request, &reference)?;
            reference
        } else {
            self.create_checkpoint_inner(request, store, Some((participant, request_fingerprint)))
                .await?
        };
        // Deletion, including response-loss replay, is the final external
        // mutation under the same Ready-or-Removing participant lock and live
        // terminal authorization.
        participant.validate_before_effect()?;
        store.delete(&reference.id).await
    }

    pub(super) async fn create_checkpoint(
        &self,
        request: &pc::SandboxCheckpointRequest,
        store: &dyn pc::SandboxCheckpointStore,
    ) -> Result<pc::SandboxCheckpointRef, pc::SandboxError> {
        if self.realization.current().is_some() {
            return Err(err(
                "current filesystem sandbox checkpoint requires an aggregate effect fence",
            ));
        }
        self.create_checkpoint_inner(request, store, None).await
    }

    pub(super) async fn create_checkpoint_for_effect(
        &self,
        request: &pc::SandboxCheckpointRequest,
        store: &dyn pc::SandboxCheckpointStore,
        effect_fence: &pc::SandboxEffectFence,
    ) -> Result<pc::SandboxCheckpointRef, pc::SandboxError> {
        validate_checkpoint_effect(request, effect_fence)?;
        let request_fingerprint = checkpoint_request_fingerprint(request, effect_fence);
        let evidence = self
            .realization
            .current()
            .ok_or_else(|| err("legacy filesystem sandbox cannot publish a fenced checkpoint"))?;
        let mut operation = crate::realization_marker::begin_ready_operation(
            self.root.root(),
            evidence,
            effect_fence,
        )?;
        if let Some(reference) = operation.completed_checkpoint(&request_fingerprint)? {
            validate_checkpoint_reference(request, &reference)?;
            return Ok(reference);
        }
        self.create_checkpoint_inner(
            request,
            store,
            Some((&mut operation, request_fingerprint.as_str())),
        )
        .await
    }

    pub(super) async fn cleanup_checkpoint_for_terminal(
        &self,
        request: &pc::SandboxCheckpointRequest,
        store: &dyn pc::SandboxCheckpointStore,
        expected_effect_fence: &pc::SandboxEffectFence,
        terminal_effect_fence: &pc::SandboxEffectFence,
    ) -> Result<(), pc::SandboxError> {
        validate_checkpoint_effect(request, expected_effect_fence)?;
        let request_fingerprint = checkpoint_request_fingerprint(request, expected_effect_fence);
        let removal = self
            .terminal_removal
            .lock()
            .map_err(|_| err("terminal removal lock poisoned"))?
            .take();
        if let Some(mut removal) = removal {
            let result = async {
                removal.refresh_authorization(terminal_effect_fence)?;
                removal.bind_checkpoint_expected(expected_effect_fence)?;
                self.cleanup_checkpoint_with_participant(
                    request,
                    store,
                    request_fingerprint.as_str(),
                    &mut removal,
                )
                .await
            }
            .await;
            *self
                .terminal_removal
                .lock()
                .map_err(|_| err("terminal removal lock poisoned while retaining participant"))? =
                Some(removal);
            return result;
        }

        let evidence = self
            .realization
            .current()
            .ok_or_else(|| err("legacy filesystem sandbox cannot recover a fenced checkpoint"))?;
        let mut ready = crate::realization_marker::begin_ready_terminal_checkpoint(
            self.root.root(),
            evidence,
            expected_effect_fence,
            terminal_effect_fence,
        )?;
        self.cleanup_checkpoint_with_participant(
            request,
            store,
            request_fingerprint.as_str(),
            &mut ready,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(effect_id: &str) -> pc::SandboxCheckpointRequest {
        pc::SandboxCheckpointRequest {
            workspace_id: "workspace".into(),
            session_id: "session".into(),
            generation_id: "generation".into(),
            environment_fingerprint: "environment".into(),
            base_image_fingerprint: "base".into(),
            effect_id: effect_id.into(),
            format: "awaken-fs-tar-v1".into(),
            created_at_unix_ms: 1,
            expires_at_unix_ms: u64::MAX,
            max_bytes: u64::MAX,
        }
    }

    #[test]
    fn checkpoint_archive_and_effect_identity_decision_table_is_total() {
        // Cause/effect table: C1 exact snapshot entry is empty-dir/regular-file;
        // C2 request effect id equals/differs from the live Suspend fence; C3
        // durable put response is delivered/lost. R1 C1 encodes only captured
        // descriptor bytes and owner mode, including empty dirs; R2 equal ids
        // admit and repeated validation after response loss remains the same
        // operation; R3 differing ids reject before snapshot/store. Marker phase,
        // root, incarnation, expiry, and terminal collision combinations are
        // owned by `ready_operation_admission_holds_one_exact_effect_boundary`.
        let snapshot = awaken_sandbox_fs::TreeSnapshot {
            directories: vec![awaken_sandbox_fs::TreeDirectory {
                relative_path: "empty".into(),
                mode: 0o700,
            }],
            files: vec![awaken_sandbox_fs::TreeFile {
                relative_path: "nested/file".into(),
                bytes: b"checkpoint".to_vec(),
                mode: 0o600,
            }],
        };
        let mut bytes = Vec::new();
        {
            let mut archive = tar::Builder::new(&mut bytes);
            append_snapshot(&mut archive, &snapshot).expect("R1");
            archive.finish().unwrap();
        }
        let mut archive = tar::Archive::new(std::io::Cursor::new(bytes));
        let mut entries = archive.entries().unwrap();
        let empty = entries.next().unwrap().unwrap();
        assert_eq!(empty.path().unwrap().as_ref(), Path::new("empty"), "R1");
        assert!(empty.header().entry_type().is_dir(), "R1");
        let mut file = entries.next().unwrap().unwrap();
        assert_eq!(
            file.path().unwrap().as_ref(),
            Path::new("nested/file"),
            "R1"
        );
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"checkpoint", "R1");

        let suspend =
            pc::SandboxEffectFence::new("suspend", "owner", "runtime", 1, u64::MAX).unwrap();
        validate_checkpoint_effect(&request("suspend"), &suspend).expect("R2");
        validate_checkpoint_effect(&request("suspend"), &suspend).expect("R2 replay");
        assert!(
            validate_checkpoint_effect(&request("other"), &suspend).is_err(),
            "R3"
        );
    }
}
