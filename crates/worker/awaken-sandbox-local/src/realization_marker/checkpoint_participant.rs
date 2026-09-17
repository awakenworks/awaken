use super::*;

impl ReadyOperationGuard {
    pub(super) fn current_marker(&self) -> Result<RealizationMarker, pc::SandboxError> {
        let marker = read_marker_locked(&self.root, &self.lock)?.ok_or_else(|| {
            pc::SandboxError::gone("ready filesystem operation lost its realization marker")
        })?;
        validate_ready_operation_marker(
            &self.root,
            &marker,
            &self.evidence,
            &self.operation_effect_fence,
            &self.authorization_effect_fence,
            &self.marker.effect_fence,
            &self.lock,
        )?;
        if marker != self.marker {
            return Err(pc::SandboxError::conflict(
                "ready filesystem operation marker changed outside its held lifecycle lock",
            ));
        }
        Ok(marker)
    }

    /// Keep response-loss Memory replay bound to the same immutable Ready
    /// completion receipt observed before this lock was acquired. The live
    /// effect and exact root are rechecked at every external mount boundary.
    pub(crate) fn validate_receipt_before_effect(
        &self,
        expected: &RealizationCompletionReceipt,
    ) -> Result<(), pc::SandboxError> {
        // The caller enters current Worker/Session authority after every wait;
        // this participant owns only exact marker/root/receipt identity.
        execution_participant::require_quiescent(&self.current_marker()?)?;
        if self.marker.completion.as_ref() != Some(expected) {
            return Err(pc::SandboxError::conflict(
                "ready filesystem effect changed the exact completion receipt",
            ));
        }
        Ok(())
    }
}

fn completed_checkpoint(
    marker: &RealizationMarker,
    request_fingerprint: &str,
    operation_id: &str,
) -> Result<Option<pc::SandboxCheckpointRef>, pc::SandboxError> {
    let Some(participant) = &marker.checkpoint else {
        return Ok(None);
    };
    participant.completed_for(request_fingerprint, operation_id)
}

fn begin_checkpoint_upload(
    root: &Path,
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
    marker: &mut RealizationMarker,
    pending: pc::SandboxCheckpointUpload,
) -> Result<Option<pc::SandboxCheckpointRef>, pc::SandboxError> {
    if let Some(participant) = &marker.checkpoint {
        if !participant.matches_pending(&pending) {
            return Err(pc::SandboxError::conflict(
                "filesystem checkpoint replay changed request metadata or snapshot bytes",
            ));
        }
        return Ok(participant.reference().cloned());
    }
    let mut next = marker.clone();
    next.checkpoint = Some(pending);
    replace_marker_locked(root, lock, marker, &next)?;
    *marker = next;
    Ok(None)
}

fn complete_checkpoint_upload(
    root: &Path,
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
    marker: &mut RealizationMarker,
    operation_id: &str,
    request_fingerprint: &str,
    reference: &pc::SandboxCheckpointRef,
) -> Result<(), pc::SandboxError> {
    let mut next = marker.clone();
    let participant = next.checkpoint.as_mut().ok_or_else(|| {
        pc::SandboxError::not_found("filesystem checkpoint completion has no pending participant")
    })?;
    if !participant.complete(operation_id, request_fingerprint, reference)? {
        return Ok(());
    }
    replace_marker_locked(root, lock, marker, &next)?;
    *marker = next;
    Ok(())
}

/// One checkpoint participant API shared by Ready upload and Removing terminal
/// cleanup. Implementations differ only in phase/evidence validation; WAL
/// transitions and exact replay comparisons above have one owner.
/// This private port proves structural custody, not liveness. Production calls
/// must be wrapped by CurrentCheckpointParticipant using the original current
/// execution authority, including cached receipt reads and final success.
pub(crate) trait CheckpointParticipantGuard: Send {
    fn completed_checkpoint(
        &self,
        request_fingerprint: &str,
    ) -> Result<Option<pc::SandboxCheckpointRef>, pc::SandboxError>;

    fn begin_checkpoint_upload(
        &mut self,
        request_fingerprint: &str,
        snapshot_digest: &str,
        snapshot_size_bytes: u64,
    ) -> Result<Option<pc::SandboxCheckpointRef>, pc::SandboxError>;

    fn complete_checkpoint_upload(
        &mut self,
        request_fingerprint: &str,
        reference: &pc::SandboxCheckpointRef,
    ) -> Result<(), pc::SandboxError>;

    fn validate_before_effect(&self) -> Result<(), pc::SandboxError>;
}

impl CheckpointParticipantGuard for ReadyOperationGuard {
    // Exact receipt/WAL identity belongs here; current liveness belongs to the
    // provider's existing CurrentCheckpointParticipant around every call.
    fn completed_checkpoint(
        &self,
        request_fingerprint: &str,
    ) -> Result<Option<pc::SandboxCheckpointRef>, pc::SandboxError> {
        let marker = self.current_marker()?;
        completed_checkpoint(
            &marker,
            request_fingerprint,
            &self.operation_effect_fence.operation_id,
        )
    }

    /// Publish or replay the pending snapshot evidence before object mutation.
    fn begin_checkpoint_upload(
        &mut self,
        request_fingerprint: &str,
        snapshot_digest: &str,
        snapshot_size_bytes: u64,
    ) -> Result<Option<pc::SandboxCheckpointRef>, pc::SandboxError> {
        self.current_marker()?;
        let operation_id = self.operation_effect_fence.operation_id.clone();
        let pending = pc::SandboxCheckpointUpload::pending(
            operation_id,
            request_fingerprint,
            snapshot_digest,
            snapshot_size_bytes,
        );
        begin_checkpoint_upload(&self.root, &self.lock, &mut self.marker, pending)
    }

    /// Persist the verified durability receipt before returning it to Session.
    fn complete_checkpoint_upload(
        &mut self,
        request_fingerprint: &str,
        reference: &pc::SandboxCheckpointRef,
    ) -> Result<(), pc::SandboxError> {
        self.current_marker()?;
        let operation_id = self.operation_effect_fence.operation_id.clone();
        complete_checkpoint_upload(
            &self.root,
            &self.lock,
            &mut self.marker,
            &operation_id,
            request_fingerprint,
            reference,
        )
    }

    fn validate_before_effect(&self) -> Result<(), pc::SandboxError> {
        execution_participant::require_quiescent(&self.current_marker()?)
    }
}

impl CheckpointParticipantGuard for RemovalGuard {
    fn completed_checkpoint(
        &self,
        request_fingerprint: &str,
    ) -> Result<Option<pc::SandboxCheckpointRef>, pc::SandboxError> {
        let marker = self.current_checkpoint_marker()?;
        let operation_id = self
            .checkpoint_expected_effect_fence
            .as_ref()
            .ok_or_else(|| {
                pc::SandboxError::not_found(
                    "terminal checkpoint participant has no bound Suspend effect",
                )
            })?;
        completed_checkpoint(&marker, request_fingerprint, &operation_id.operation_id)
    }

    fn begin_checkpoint_upload(
        &mut self,
        request_fingerprint: &str,
        snapshot_digest: &str,
        snapshot_size_bytes: u64,
    ) -> Result<Option<pc::SandboxCheckpointRef>, pc::SandboxError> {
        self.current_checkpoint_marker()?;
        let operation_id = self
            .checkpoint_expected_effect_fence
            .as_ref()
            .ok_or_else(|| {
                pc::SandboxError::not_found(
                    "terminal checkpoint participant has no bound Suspend effect",
                )
            })?
            .operation_id
            .clone();
        let pending = pc::SandboxCheckpointUpload::pending(
            operation_id,
            request_fingerprint,
            snapshot_digest,
            snapshot_size_bytes,
        );
        begin_checkpoint_upload(&self.root, &self.lock, &mut self.marker, pending)
    }

    fn complete_checkpoint_upload(
        &mut self,
        request_fingerprint: &str,
        reference: &pc::SandboxCheckpointRef,
    ) -> Result<(), pc::SandboxError> {
        self.current_checkpoint_marker()?;
        let operation_id = self
            .checkpoint_expected_effect_fence
            .as_ref()
            .ok_or_else(|| {
                pc::SandboxError::not_found(
                    "terminal checkpoint participant has no bound Suspend effect",
                )
            })?
            .operation_id
            .clone();
        complete_checkpoint_upload(
            &self.root,
            &self.lock,
            &mut self.marker,
            &operation_id,
            request_fingerprint,
            reference,
        )
    }

    fn validate_before_effect(&self) -> Result<(), pc::SandboxError> {
        self.current_checkpoint_marker().map(|_| ())
    }
}
