// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
//! Portable filesystem checkpoint encoding for the local provider.

use std::io::{Read as _, Write};
use std::path::Path;

use super::{LocalProvider, LocalSandbox, pc};
use crate::realization_marker::CheckpointParticipantGuard;
use awaken_resource_contract::content_id;

/// Borrow the original Ready/Removing participant through the shared effect
/// authority. This adapter owns no lock, journal, lease or cached receipt.
/// Every synchronous WAL decision stays inside the same current guard; the
/// existing upload algorithm therefore re-enters it after an awaited put.
struct CurrentCheckpointParticipant<'a> {
    participant: &'a mut dyn CheckpointParticipantGuard,
    fence: &'a pc::SandboxEffectFence,
    authority: &'a dyn pc::CurrentExecutionAuthority,
}

impl CheckpointParticipantGuard for CurrentCheckpointParticipant<'_> {
    fn completed_checkpoint(
        &self,
        request_fingerprint: &str,
    ) -> Result<Option<pc::SandboxCheckpointRef>, pc::SandboxError> {
        pc::with_current_sandbox_effect(self.fence, self.authority, |_| {
            self.participant.completed_checkpoint(request_fingerprint)
        })
    }

    fn begin_checkpoint_upload(
        &mut self,
        request_fingerprint: &str,
        snapshot_digest: &str,
        snapshot_size_bytes: u64,
    ) -> Result<Option<pc::SandboxCheckpointRef>, pc::SandboxError> {
        pc::with_current_sandbox_effect(self.fence, self.authority, |_| {
            self.participant.begin_checkpoint_upload(
                request_fingerprint,
                snapshot_digest,
                snapshot_size_bytes,
            )
        })
    }

    fn complete_checkpoint_upload(
        &mut self,
        request_fingerprint: &str,
        reference: &pc::SandboxCheckpointRef,
    ) -> Result<(), pc::SandboxError> {
        pc::with_current_sandbox_effect(self.fence, self.authority, |_| {
            self.participant
                .complete_checkpoint_upload(request_fingerprint, reference)
        })
    }

    fn validate_before_effect(&self) -> Result<(), pc::SandboxError> {
        pc::with_current_sandbox_effect(self.fence, self.authority, |_| {
            self.participant.validate_before_effect()
        })
    }
}

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
            .map_err(|error| pc::SandboxError::backend("append_snapshot", error))?;
    }
    for file in &snapshot.files {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(file.mode);
        header.set_size(u64::try_from(file.bytes.len()).unwrap_or(u64::MAX));
        header.set_cksum();
        archive
            .append_data(&mut header, &file.relative_path, file.bytes.as_slice())
            .map_err(|error| pc::SandboxError::backend("append_snapshot", error))?;
    }
    Ok(())
}

fn validate_checkpoint_effect(
    request: &pc::SandboxCheckpointCaptureRequest,
    effect_fence: &pc::SandboxEffectFence,
) -> Result<(), pc::SandboxError> {
    if request.effect_id == effect_fence.operation_id() {
        Ok(())
    } else {
        Err(pc::SandboxError::fence_rejected(
            "checkpoint request effect id does not match its Sandbox effect fence",
        ))
    }
}

fn checkpoint_request_fingerprint(
    request: &pc::SandboxCheckpointCaptureRequest,
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
        effect_fence.operation_id(),
        effect_fence.owner(),
        effect_fence.runtime_incarnation(),
    ] {
        hasher.update(&(value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    for value in [
        request.created_at_unix_ms,
        request.expires_at_unix_ms,
        request.max_bytes,
        effect_fence.epoch(),
    ] {
        hasher.update(&value.to_be_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn validate_checkpoint_reference(
    request: &pc::SandboxCheckpointCaptureRequest,
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
        Err(pc::SandboxError::conflict(
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
            return Err(pc::SandboxError::unsupported(format!(
                "unsupported checkpoint format {:?}",
                request.checkpoint.format
            )));
        }
        let target = self.acquire_restore_sandbox(spec, request).await?;
        let evidence = target.evidence().clone();
        let sandbox = target.into_target();
        let bytes = store.get(&request.checkpoint.id).await?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != request.checkpoint.size_bytes
            || content_id(&bytes) != request.checkpoint.digest
        {
            return Err(pc::SandboxError::conflict(
                "checkpoint size or digest mismatch",
            ));
        }

        let _target_lock =
            super::restore_target::lock_bound_target(sandbox.root.root(), &evidence).await?;
        let root_identity = awaken_sandbox_fs::directory_identity_nofollow(sandbox.root.root())
            .map_err(|error| pc::SandboxError::backend("restore_exact_sandbox", error))?;
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
    for entry in archive
        .entries()
        .map_err(|error| pc::SandboxError::backend("extract_checkpoint", error))?
    {
        let mut entry =
            entry.map_err(|error| pc::SandboxError::backend("extract_checkpoint", error))?;
        let path = entry
            .path()
            .map_err(|error| pc::SandboxError::backend("extract_checkpoint", error))?
            .into_owned();
        if path.as_os_str().is_empty()
            || path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(pc::SandboxError::invalid(format!(
                "checkpoint entry `{}` is not one canonical relative path",
                path.display()
            )));
        }
        if !seen.insert(path.clone()) {
            return Err(pc::SandboxError::invalid(format!(
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
            return Err(pc::SandboxError::invalid(format!(
                "checkpoint entry `{}` overlaps an independently governed mount",
                path.display()
            )));
        }
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            let mode = entry
                .header()
                .mode()
                .map_err(|error| pc::SandboxError::backend("extract_checkpoint", error))?
                & 0o777;
            validate_before_mutation()?;
            awaken_sandbox_fs::create_relative_directory_all(root, root_identity, &path)
                .map_err(|error| pc::SandboxError::backend("extract_checkpoint", error))?;
            directory_modes.push((path, mode));
        } else if entry_type.is_file() {
            let mut contents = Vec::new();
            entry
                .read_to_end(&mut contents)
                .map_err(|error| pc::SandboxError::backend("extract_checkpoint", error))?;
            let mode = entry
                .header()
                .mode()
                .map_err(|error| pc::SandboxError::backend("extract_checkpoint", error))?
                & 0o777;
            validate_before_mutation()?;
            awaken_sandbox_fs::write_relative_file_atomic(
                root,
                root_identity,
                &path,
                &contents,
                mode,
            )
            .map_err(|error| pc::SandboxError::backend("extract_checkpoint", error))?;
        } else {
            return Err(pc::SandboxError::invalid(format!(
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
            .map_err(|error| pc::SandboxError::backend("extract_checkpoint", error))?;
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
                    pc::SandboxError::invalid(format!(
                        "checkpoint exclusion `{}` is outside sandbox root `{}`",
                        path.display(),
                        self.root.root().display()
                    ))
                })?;
                if relative.as_os_str().is_empty() {
                    return Err(pc::SandboxError::invalid(
                        "checkpoint cannot exclude the complete sandbox root",
                    ));
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
        .map_err(|error| pc::SandboxError::backend("checkpoint_snapshot", error))?;

        let mut bytes = Vec::new();
        {
            let mut archive = tar::Builder::new(&mut bytes);
            append_snapshot(&mut archive, &snapshot)?;
            archive
                .finish()
                .map_err(|error| pc::SandboxError::backend("checkpoint_snapshot", error))?;
        }
        Ok(bytes)
    }

    async fn create_checkpoint_inner(
        &self,
        request: &pc::SandboxCheckpointCaptureRequest,
        store: &dyn pc::SandboxCheckpointStore,
        mut operation: Option<(
            &mut dyn crate::realization_marker::CheckpointParticipantGuard,
            &str,
        )>,
    ) -> Result<pc::SandboxCheckpointRef, pc::SandboxError> {
        if request.format != "awaken-fs-tar-v1" {
            return Err(pc::SandboxError::unsupported(format!(
                "unsupported checkpoint format {:?}",
                request.format
            )));
        }
        if let Some((operation, _)) = operation.as_ref() {
            operation.validate_before_effect()?;
        }
        let bytes = self.checkpoint_snapshot()?;
        let size_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if size_bytes > request.max_bytes {
            return Err(pc::SandboxError::invalid(format!(
                "checkpoint is {size_bytes} bytes, limit is {}",
                request.max_bytes
            )));
        }
        let digest = content_id(&bytes);
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
            return Err(pc::SandboxError::conflict(
                "checkpoint store durability receipt mismatch",
            ));
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
        request: &pc::SandboxCheckpointCaptureRequest,
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
        store.delete(&reference.id).await?;
        // A completed backend delete is not permission to acknowledge cleanup.
        // On denial/cancellation, the original WAL still owns exact replay.
        participant.validate_before_effect()
    }

    /// Test fixture: checkpoint bytes of a marker-free sandbox that no
    /// aggregate effect owns. Production checkpoints are always effect-fenced.
    #[cfg(test)]
    pub(crate) async fn create_checkpoint(
        &self,
        request: &pc::SandboxCheckpointCaptureRequest,
        store: &dyn pc::SandboxCheckpointStore,
    ) -> Result<pc::SandboxCheckpointRef, pc::SandboxError> {
        if self.realization.current().is_some() {
            return Err(pc::SandboxError::fence_rejected(
                "current filesystem sandbox checkpoint requires an aggregate effect fence",
            ));
        }
        self.create_checkpoint_inner(request, store, None).await
    }

    pub(super) async fn create_checkpoint_for_effect(
        &self,
        request: &pc::SandboxCheckpointCaptureRequest,
        store: &dyn pc::SandboxCheckpointStore,
        effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<pc::SandboxCheckpointRef, pc::SandboxError> {
        pc::with_current_sandbox_effect(effect_fence, authority, |_| Ok(()))?;
        validate_checkpoint_effect(request, effect_fence)?;
        let request_fingerprint = checkpoint_request_fingerprint(request, effect_fence);
        let evidence = self.realization.current().ok_or_else(|| {
            pc::SandboxError::unsupported(
                "legacy filesystem sandbox cannot publish a fenced checkpoint",
            )
        })?;
        let mut operation = crate::realization_marker::begin_ready_operation(
            self.root.root(),
            evidence,
            effect_fence,
        )?;
        let mut operation = CurrentCheckpointParticipant {
            participant: &mut operation,
            fence: effect_fence,
            authority,
        };
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
        request: &pc::SandboxCheckpointCaptureRequest,
        store: &dyn pc::SandboxCheckpointStore,
        expected_effect_fence: &pc::SandboxEffectFence,
        terminal_effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError> {
        pc::with_current_sandbox_effect(terminal_effect_fence, authority, |_| Ok(()))?;
        validate_checkpoint_effect(request, expected_effect_fence)?;
        let request_fingerprint = checkpoint_request_fingerprint(request, expected_effect_fence);
        let mut slot = self.terminal_removal.lock().await;
        if let Some(removal) = slot.as_mut() {
            pc::with_current_sandbox_effect(terminal_effect_fence, authority, |admitted| {
                removal.refresh_authorization(terminal_effect_fence, admitted)?;
                removal.bind_checkpoint_expected(expected_effect_fence)
            })?;
            return self
                .cleanup_checkpoint_with_participant(
                    request,
                    store,
                    request_fingerprint.as_str(),
                    &mut CurrentCheckpointParticipant {
                        participant: removal,
                        fence: terminal_effect_fence,
                        authority,
                    },
                )
                .await;
        }

        let evidence = self.realization.current().ok_or_else(|| {
            pc::SandboxError::unsupported(
                "legacy filesystem sandbox cannot recover a fenced checkpoint",
            )
        })?;
        let mut ready = pc::with_current_sandbox_effect(terminal_effect_fence, authority, |_| {
            crate::realization_marker::begin_ready_terminal_checkpoint(
                self.root.root(),
                evidence,
                expected_effect_fence,
                terminal_effect_fence,
            )
        })?;
        self.cleanup_checkpoint_with_participant(
            request,
            store,
            request_fingerprint.as_str(),
            &mut CurrentCheckpointParticipant {
                participant: &mut ready,
                fence: terminal_effect_fence,
                authority,
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::super::workdir_helper_tests::{CheckpointPause, CheckpointStore, workdir_spec};
    use super::*;
    use awaken_provisioning_contract::test_support::FenceTamper as _;
    use pc::test_support::EffectAuthority;
    use std::sync::{Arc, atomic::Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn checkpoint_upload_and_cached_receipt_require_current_authority_not_utc() {
        // Decision table U: C1 past/future Control expiry; C2 new/cached WAL;
        // C3 current/revoked/exhausted/wrong-operation/wrong-generation proof.
        // 2x2x5 rows. U1 current -> one upload or exact cached receipt, unchanged
        // metadata. U2 all other proofs -> zero NEW WAL/store effects, including
        // a cached receipt. U3 source bytes and original WAL survive denial.
        // All structural coordinates besides the selected cause remain valid.
        for expiry in [100, u64::MAX - 1] {
            for cached in [false, true] {
                for change in ["current", "revoke", "expire", "operation", "generation"] {
                    let tmp = tempfile::tempdir().unwrap();
                    let create =
                        pc::SandboxEffectFence::new("create", "owner", "boot", 1, u64::MAX)
                            .unwrap();
                    let suspend =
                        pc::SandboxEffectFence::new("suspend", "owner", "boot", 2, expiry).unwrap();
                    let sandbox = LocalProvider::new(tmp.path())
                        .create_sandbox_for_effect(
                            &workdir_spec("upload-clock", false),
                            &create,
                            None,
                            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                                &create,
                            ),
                        )
                        .await
                        .unwrap();
                    let path = sandbox.workspace_path().join("source");
                    std::fs::write(&path, b"unchanged").unwrap();
                    let request = request("suspend");
                    let store = CheckpointStore::default();
                    let authority = EffectAuthority::new(&suspend);
                    let original = if cached {
                        Some(
                            pc::CheckpointingSandbox::checkpoint_for_effect(
                                &sandbox, &request, &store, &suspend, &authority,
                            )
                            .await
                            .unwrap(),
                        )
                    } else {
                        None
                    };
                    match change {
                        "revoke" => *authority.current.lock().unwrap() = None,
                        "expire" => {
                            authority.current.lock().unwrap().as_mut().unwrap().1 = Duration::ZERO
                        }
                        "operation" => {
                            let mut guard = authority.current.lock().unwrap();
                            guard
                                .as_mut()
                                .unwrap()
                                .0
                                .tamper(|w| w.operation_id.push('x'));
                        }
                        "generation" => {
                            let mut guard = authority.current.lock().unwrap();
                            guard.as_mut().unwrap().0.reassign();
                        }
                        _ => {}
                    }
                    let result = pc::CheckpointingSandbox::checkpoint_for_effect(
                        &sandbox, &request, &store, &suspend, &authority,
                    )
                    .await;
                    assert_eq!(
                        result.is_ok(),
                        change == "current",
                        "U {expiry}/{cached}/{change}: {result:?}"
                    );
                    assert_eq!(
                        store.puts.load(Ordering::SeqCst),
                        usize::from(cached || change == "current"),
                        "U1/U2"
                    );
                    assert_eq!(store.deletes.load(Ordering::SeqCst), 0);
                    assert_eq!(std::fs::read(path).unwrap(), b"unchanged", "U3");
                    let guard = crate::realization_marker::begin_ready_operation(
                        sandbox.root.root(),
                        sandbox.realization.current().unwrap(),
                        &suspend,
                    )
                    .unwrap();
                    let recorded = guard
                        .completed_checkpoint(&checkpoint_request_fingerprint(&request, &suspend))
                        .unwrap();
                    assert_eq!(recorded.is_some(), cached || change == "current", "U3 WAL");
                    if let Ok(receipt) = result {
                        assert_eq!(recorded.as_ref(), Some(&receipt));
                        assert_eq!(receipt.created_at_unix_ms, request.created_at_unix_ms);
                        assert_eq!(receipt.expires_at_unix_ms, request.expires_at_unix_ms);
                        if let Some(original) = original {
                            assert_eq!(receipt, original, "U1 exact replay");
                        }
                    } else {
                        assert_eq!(recorded, original);
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn checkpoint_put_response_cannot_publish_after_authority_loss() {
        // Decision table UP: C1 past/future Control expiry; C2 revoke/replace/
        // exhaust/renew/cancel while the real store has committed a put but
        // withholds its response. UP1 renewal -> original WAL receipt once.
        // UP2 all other rows -> retained pending WAL, no success/receipt/delete.
        // UP3 exact retry reuses the SAME object key and bytes, then cached
        // replay never uploads again. An admitted remote put is not rolled back.
        // No sleep or remote-commit fencing claim; the existing store owns CAS.
        for expiry in [100, u64::MAX - 1] {
            for change in ["revoke", "replace", "expire", "renew", "cancel"] {
                let tmp = tempfile::tempdir().unwrap();
                let create =
                    pc::SandboxEffectFence::new("create", "owner", "boot", 1, u64::MAX).unwrap();
                let suspend =
                    pc::SandboxEffectFence::new("suspend", "owner", "boot", 2, expiry).unwrap();
                let sandbox = LocalProvider::new(tmp.path())
                    .create_sandbox_for_effect(
                        &workdir_spec("upload-wait", false),
                        &create,
                        None,
                        &awaken_provisioning_contract::test_support::EffectAuthority::new(&create),
                    )
                    .await
                    .unwrap();
                let request = request("suspend");
                let store = CheckpointStore::default();
                let authority = EffectAuthority::new(&suspend);
                let gate = Arc::new(tokio::sync::Notify::new());
                *store.pause.lock().unwrap() = Some((CheckpointPause::AfterPut, gate.clone()));
                let mut pending = Box::pin(pc::CheckpointingSandbox::checkpoint_for_effect(
                    &sandbox, &request, &store, &suspend, &authority,
                ));
                std::future::poll_fn(|cx| {
                    assert!(
                        std::future::Future::poll(pending.as_mut(), cx).is_pending(),
                        "UP must reach actual Pending"
                    );
                    std::task::Poll::Ready(())
                })
                .await;
                assert_eq!(store.puts.load(Ordering::SeqCst), 1);
                assert_eq!(store.objects.lock().unwrap().len(), 1);
                match change {
                    "revoke" => *authority.current.lock().unwrap() = None,
                    "replace" => {
                        let mut guard = authority.current.lock().unwrap();
                        guard.as_mut().unwrap().0.reassign();
                    }
                    "expire" => {
                        authority.current.lock().unwrap().as_mut().unwrap().1 = Duration::ZERO
                    }
                    "renew" => {
                        let mut guard = authority.current.lock().unwrap();
                        guard.as_mut().unwrap().0.renew();
                    }
                    _ => {}
                }
                if change == "cancel" {
                    drop(pending);
                } else {
                    gate.notify_one();
                    assert_eq!(
                        pending.await.is_ok(),
                        change == "renew",
                        "UP {expiry}/{change}"
                    );
                }
                let fingerprint = checkpoint_request_fingerprint(&request, &suspend);
                let guard = crate::realization_marker::begin_ready_operation(
                    sandbox.root.root(),
                    sandbox.realization.current().unwrap(),
                    &suspend,
                )
                .unwrap();
                assert_eq!(
                    guard.completed_checkpoint(&fingerprint).unwrap().is_some(),
                    change == "renew",
                    "UP2 no late receipt"
                );
                drop(guard);
                *store.pause.lock().unwrap() = None;
                // Explicit test recovery, not revival of production revoked roots.
                *authority.current.lock().unwrap() =
                    Some((suspend.clone(), Duration::from_secs(1)));
                let receipt = pc::CheckpointingSandbox::checkpoint_for_effect(
                    &sandbox, &request, &store, &suspend, &authority,
                )
                .await
                .unwrap();
                let replay = pc::CheckpointingSandbox::checkpoint_for_effect(
                    &sandbox, &request, &store, &suspend, &authority,
                )
                .await
                .unwrap();
                assert_eq!(receipt, replay, "UP3");
                assert_eq!(
                    store.puts.load(Ordering::SeqCst),
                    if change == "renew" { 1 } else { 2 }
                );
                assert_eq!(
                    store.objects.lock().unwrap().len(),
                    1,
                    "UP3 no orphan second object"
                );
                assert_eq!(store.deletes.load(Ordering::SeqCst), 0);
            }
        }
    }

    #[tokio::test]
    async fn terminal_checkpoint_current_authority_covers_every_wait_and_exact_retry() {
        // Cause/effect graph: C1 original participant Ready/Removing; C2 wait
        // slot/committed-put-response/committed-delete-response; C3 current
        // authority revoked/replaced/expired/renewed/caller-cancelled.
        // Cross past/future Control timestamps: 2 x 2 x 3 x 5 = 60 real
        // Pending schedules, not sleeps or pre-set results.
        // R1 denied entry => zero WAL/store effect. R2 denial after slot/put =>
        // no next put/receipt/delete; after delete => no success (the already
        // committed delete is not undone). R3 renewal => exact completion.
        // R4 cancellation preserves the original Removing guard or Ready WAL.
        // R5 exact retry after a lost put response repeats the SAME object key,
        // while receipt replay never uploads again. R6 cached success remains
        // authority-bound. Byte/root identity survives every failed attempt.
        // Store atomicity and remote cancellation are not proved. Fresh local
        // proof must admit both timestamp classes, without shifting WAL identity.
        for (removing, expiry) in [false, true]
            .into_iter()
            .flat_map(|removing| [100, u64::MAX - 1].map(|expiry| (removing, expiry)))
        {
            for wait in ["slot", "put", "delete"] {
                for change in ["revoke", "replace", "expire", "renew", "cancel"] {
                    let tmp = tempfile::tempdir().unwrap();
                    let spec = workdir_spec("current-checkpoint", false);
                    let provider = LocalProvider::new(tmp.path());
                    let create =
                        pc::SandboxEffectFence::new("create", "owner", "runtime", 1, u64::MAX - 1)
                            .unwrap();
                    let suspend =
                        pc::SandboxEffectFence::new("suspend", "owner", "runtime", 2, expiry)
                            .unwrap();
                    let terminal =
                        pc::SandboxEffectFence::new("terminal", "owner", "runtime", 3, expiry)
                            .unwrap();
                    let sandbox = provider
                        .create_sandbox_for_effect(
                            &spec,
                            &create,
                            None,
                            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                                &create,
                            ),
                        )
                        .await
                        .unwrap();
                    let request = request("suspend");
                    let authority = EffectAuthority::new(&terminal);
                    if removing {
                        pc::Sandbox::prepare_disposal_for_effect(&sandbox, &terminal, &authority)
                            .await
                            .unwrap();
                    }
                    let path = sandbox.workspace_path().join("unchanged");
                    std::fs::write(&path, b"checkpoint-source").unwrap();
                    let store = CheckpointStore::default();
                    assert!(
                        pc::CheckpointingSandbox::cleanup_checkpoint_for_terminal(
                            &sandbox,
                            &request,
                            &store,
                            &suspend,
                            &terminal,
                            &EffectAuthority::denied(),
                        )
                        .await
                        .is_err(),
                        "R1"
                    );
                    assert_eq!(store.puts.load(Ordering::SeqCst), 0, "R1");
                    assert_eq!(store.deletes.load(Ordering::SeqCst), 0, "R1");

                    let gate = Arc::new(tokio::sync::Notify::new());
                    if wait != "slot" {
                        *store.pause.lock().unwrap() = Some((
                            if wait == "put" {
                                CheckpointPause::AfterPut
                            } else {
                                CheckpointPause::AfterDelete
                            },
                            gate.clone(),
                        ));
                    }
                    let held = if wait == "slot" {
                        Some(sandbox.terminal_removal.lock().await)
                    } else {
                        None
                    };
                    let mut pending =
                        Box::pin(pc::CheckpointingSandbox::cleanup_checkpoint_for_terminal(
                            &sandbox, &request, &store, &suspend, &terminal, &authority,
                        ));
                    tokio::select! {
                        biased;
                        result = &mut pending => panic!("C2 must suspend: {result:?}"),
                        () = std::future::ready(()) => {}
                    }
                    let initial_puts = usize::from(wait != "slot");
                    let initial_deletes = usize::from(wait == "delete");
                    assert_eq!(store.puts.load(Ordering::SeqCst), initial_puts);
                    assert_eq!(store.deletes.load(Ordering::SeqCst), initial_deletes);
                    match change {
                        "revoke" => *authority.current.lock().unwrap() = None,
                        "replace" => {
                            authority.current.lock().unwrap().as_mut().unwrap().0 =
                                pc::SandboxEffectFence::new(
                                    "terminal",
                                    "replacement",
                                    "runtime",
                                    2,
                                    u64::MAX,
                                )
                                .unwrap()
                        }
                        "expire" => {
                            authority.current.lock().unwrap().as_mut().unwrap().1 = Duration::ZERO
                        }
                        "renew" => {
                            let mut guard = authority.current.lock().unwrap();
                            guard
                                .as_mut()
                                .unwrap()
                                .0
                                .tamper(|w| w.expires_at_unix_ms = u64::MAX);
                        }
                        "cancel" => {}
                        _ => unreachable!(),
                    }
                    drop(held);
                    if change == "cancel" {
                        drop(pending);
                    } else {
                        gate.notify_one();
                        assert_eq!(pending.await.is_ok(), change == "renew", "R2/R3");
                    }
                    assert_eq!(std::fs::read(&path).unwrap(), b"checkpoint-source", "R4");
                    assert_eq!(
                        sandbox.terminal_removal.lock().await.is_some(),
                        removing,
                        "R4 original participant is never taken out across I/O",
                    );
                    if change != "renew" {
                        assert_eq!(store.puts.load(Ordering::SeqCst), initial_puts, "R2");
                        assert_eq!(store.deletes.load(Ordering::SeqCst), initial_deletes, "R2");
                        assert_eq!(
                            store.objects.lock().unwrap().len(),
                            usize::from(wait == "put")
                        );
                        // Inspect the original journal, independently of the
                        // current-authority adapter, before the retry can fix it.
                        let fingerprint = checkpoint_request_fingerprint(&request, &suspend);
                        let recorded = if removing && wait == "slot" {
                            let slot = sandbox.terminal_removal.lock().await;
                            let error = slot
                                .as_ref()
                                .unwrap()
                                .completed_checkpoint(&fingerprint)
                                .unwrap_err();
                            assert!(
                                matches!(error, pc::SandboxError::NotFound(_)),
                                "R2 slot denial cannot bind the checkpoint participant: {error}"
                            );
                            None
                        } else if removing {
                            sandbox
                                .terminal_removal
                                .lock()
                                .await
                                .as_ref()
                                .unwrap()
                                .completed_checkpoint(&fingerprint)
                                .unwrap()
                        } else {
                            crate::realization_marker::begin_ready_terminal_checkpoint(
                                sandbox.root.root(),
                                sandbox.realization.current().unwrap(),
                                &suspend,
                                &terminal,
                            )
                            .unwrap()
                            .completed_checkpoint(&fingerprint)
                            .unwrap()
                        };
                        assert_eq!(recorded.is_some(), wait == "delete", "R2 WAL boundary");
                    }
                    *store.pause.lock().unwrap() = None;
                    // Reinstall explicit test authority to inspect exact retry;
                    // this is not a production renewal of a revoked incarnation.
                    *authority.current.lock().unwrap() =
                        Some((terminal.clone(), Duration::from_secs(1)));
                    pc::CheckpointingSandbox::cleanup_checkpoint_for_terminal(
                        &sandbox, &request, &store, &suspend, &terminal, &authority,
                    )
                    .await
                    .unwrap();
                    let puts = store.puts.load(Ordering::SeqCst);
                    let deletes = store.deletes.load(Ordering::SeqCst);
                    assert_eq!(
                        puts,
                        if wait == "put" && change != "renew" {
                            2
                        } else {
                            1
                        },
                        "R5"
                    );
                    assert_eq!(
                        deletes,
                        if wait == "delete" || change == "renew" {
                            2
                        } else {
                            1
                        },
                        "R5"
                    );
                    assert!(store.objects.lock().unwrap().is_empty(), "R5");
                    *authority.current.lock().unwrap() = None;
                    assert!(
                        pc::CheckpointingSandbox::cleanup_checkpoint_for_terminal(
                            &sandbox, &request, &store, &suspend, &terminal, &authority,
                        )
                        .await
                        .is_err(),
                        "R6"
                    );
                    assert_eq!(store.puts.load(Ordering::SeqCst), puts, "R6");
                    assert_eq!(store.deletes.load(Ordering::SeqCst), deletes, "R6");
                    *authority.current.lock().unwrap() =
                        Some((terminal.clone(), Duration::from_secs(1)));
                    pc::Sandbox::prepare_disposal_for_effect(&sandbox, &terminal, &authority)
                        .await
                        .unwrap();
                    let disposal = crate::test_disposal_authorization(&terminal);
                    pc::Sandbox::dispose_for_effect(
                        &sandbox,
                        &disposal,
                        &EffectAuthority::new(disposal.effect_fence()),
                    )
                    .await
                    .unwrap();
                    assert!(
                        !path.exists(),
                        "R5 exact owned cleanup reaches its terminal state"
                    );
                }
            }
        }
    }

    fn request(effect_id: &str) -> pc::SandboxCheckpointCaptureRequest {
        pc::SandboxCheckpointCaptureRequest {
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
        // The capture request supplies immutable snapshot/object facts; it is
        // not the Session lifecycle request or a grant. R2/R3 keep its exact
        // effect admission separate from current execution authority, whose
        // loss/replay combinations are covered by the upload tests above.
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
