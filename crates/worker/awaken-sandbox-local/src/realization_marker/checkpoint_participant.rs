use super::*;

impl CheckpointParticipant {
    fn pending(
        operation_id: String,
        request_fingerprint: &str,
        snapshot_digest: &str,
        snapshot_size_bytes: u64,
    ) -> Self {
        Self {
            operation_id: Some(operation_id),
            request_fingerprint: request_fingerprint.to_owned(),
            snapshot_digest: snapshot_digest.to_owned(),
            snapshot_size_bytes,
            reference: None,
        }
    }

    fn matches_pending(&self, pending: &Self) -> bool {
        self.operation_id == pending.operation_id
            && self.request_fingerprint == pending.request_fingerprint
            && self.snapshot_digest == pending.snapshot_digest
            && self.snapshot_size_bytes == pending.snapshot_size_bytes
    }
}

impl ReadyOperationGuard {
    fn current_marker(&self) -> Result<RealizationMarker, pc::SandboxError> {
        let marker = read_marker_locked(&self.root, &self.lock)?
            .ok_or_else(|| err("ready filesystem operation lost its realization marker"))?;
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
            return Err(err(
                "ready filesystem operation marker changed outside its held lifecycle lock",
            ));
        }
        Ok(marker)
    }

    /// Return an already-recorded exact receipt before taking a second snapshot.
    /// Receipt recovery is non-mutating, so an expired lease may read it; every
    /// WAL/store mutation below still checks expiry at its own boundary.
    pub(crate) fn completed_checkpoint(
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
    pub(crate) fn begin_checkpoint_upload(
        &mut self,
        request_fingerprint: &str,
        snapshot_digest: &str,
        snapshot_size_bytes: u64,
    ) -> Result<Option<pc::SandboxCheckpointRef>, pc::SandboxError> {
        self.current_marker()?;
        let operation_id = self.operation_effect_fence.operation_id.clone();
        let pending = CheckpointParticipant::pending(
            operation_id,
            request_fingerprint,
            snapshot_digest,
            snapshot_size_bytes,
        );
        begin_checkpoint_upload(
            &self.root,
            &self.lock,
            &mut self.marker,
            &self.authorization_effect_fence,
            pending,
        )
    }

    /// Persist the verified durability receipt before returning it to Session.
    pub(crate) fn complete_checkpoint_upload(
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
            &self.authorization_effect_fence,
            &operation_id,
            request_fingerprint,
            reference,
        )
    }

    /// Recheck the lease and exact root immediately before the external effect.
    /// The lock makes this one continuation of admission; the reread also
    /// detects an out-of-protocol marker or pathname replacement.
    pub(crate) fn validate_before_effect(&self) -> Result<(), pc::SandboxError> {
        validate_live_effect_fence(&self.authorization_effect_fence)?;
        self.current_marker().map(|_| ())
    }

    /// Keep response-loss Memory replay bound to the same immutable Ready
    /// completion receipt observed before this lock was acquired. The live
    /// effect and exact root are rechecked at every external mount boundary.
    pub(crate) fn validate_receipt_before_effect(
        &self,
        expected: &RealizationCompletionReceipt,
    ) -> Result<(), pc::SandboxError> {
        self.validate_before_effect()?;
        if self.marker.completion.as_ref() != Some(expected) {
            return Err(err(
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
    if participant.request_fingerprint != request_fingerprint
        || participant.operation_id.as_deref() != Some(operation_id)
    {
        return Err(err(
            "filesystem checkpoint participant belongs to another request or operation",
        ));
    }
    Ok(participant.reference.clone())
}

fn begin_checkpoint_upload(
    root: &Path,
    lock: &awaken_sandbox_fs::ExclusiveFileLock,
    marker: &mut RealizationMarker,
    authorization_effect_fence: &pc::SandboxEffectFence,
    pending: CheckpointParticipant,
) -> Result<Option<pc::SandboxCheckpointRef>, pc::SandboxError> {
    validate_live_effect_fence(authorization_effect_fence)?;
    if let Some(participant) = &marker.checkpoint {
        if !participant.matches_pending(&pending) {
            return Err(err(
                "filesystem checkpoint replay changed request metadata or snapshot bytes",
            ));
        }
        return Ok(participant.reference.clone());
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
    authorization_effect_fence: &pc::SandboxEffectFence,
    operation_id: &str,
    request_fingerprint: &str,
    reference: &pc::SandboxCheckpointRef,
) -> Result<(), pc::SandboxError> {
    validate_live_effect_fence(authorization_effect_fence)?;
    let participant = marker
        .checkpoint
        .as_ref()
        .ok_or_else(|| err("filesystem checkpoint completion has no pending participant"))?;
    if participant.operation_id.as_deref() != Some(operation_id)
        || participant.request_fingerprint != request_fingerprint
        || participant.snapshot_digest != reference.digest
        || participant.snapshot_size_bytes != reference.size_bytes
        || reference.suspend_effect_id != operation_id
    {
        return Err(err(
            "filesystem checkpoint receipt does not match pending snapshot evidence",
        ));
    }
    if let Some(recorded) = &participant.reference {
        return if recorded == reference {
            Ok(())
        } else {
            Err(err(
                "filesystem checkpoint completion conflicts with its durable receipt",
            ))
        };
    }
    let mut next = marker.clone();
    next.checkpoint
        .as_mut()
        .expect("checkpoint participant was validated")
        .reference = Some(reference.clone());
    replace_marker_locked(root, lock, marker, &next)?;
    *marker = next;
    Ok(())
}

/// One checkpoint participant API shared by Ready upload and Removing terminal
/// cleanup. Implementations differ only in phase/evidence validation; WAL
/// transitions and exact replay comparisons above have one owner.
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
    fn completed_checkpoint(
        &self,
        request_fingerprint: &str,
    ) -> Result<Option<pc::SandboxCheckpointRef>, pc::SandboxError> {
        ReadyOperationGuard::completed_checkpoint(self, request_fingerprint)
    }

    fn begin_checkpoint_upload(
        &mut self,
        request_fingerprint: &str,
        snapshot_digest: &str,
        snapshot_size_bytes: u64,
    ) -> Result<Option<pc::SandboxCheckpointRef>, pc::SandboxError> {
        ReadyOperationGuard::begin_checkpoint_upload(
            self,
            request_fingerprint,
            snapshot_digest,
            snapshot_size_bytes,
        )
    }

    fn complete_checkpoint_upload(
        &mut self,
        request_fingerprint: &str,
        reference: &pc::SandboxCheckpointRef,
    ) -> Result<(), pc::SandboxError> {
        ReadyOperationGuard::complete_checkpoint_upload(self, request_fingerprint, reference)
    }

    fn validate_before_effect(&self) -> Result<(), pc::SandboxError> {
        ReadyOperationGuard::validate_before_effect(self)
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
            .ok_or_else(|| err("terminal checkpoint participant has no bound Suspend effect"))?;
        completed_checkpoint(&marker, request_fingerprint, &operation_id.operation_id)
    }

    fn begin_checkpoint_upload(
        &mut self,
        request_fingerprint: &str,
        snapshot_digest: &str,
        snapshot_size_bytes: u64,
    ) -> Result<Option<pc::SandboxCheckpointRef>, pc::SandboxError> {
        self.current_checkpoint_marker()?;
        let authorization = self.marker.effect_fence.clone();
        let operation_id = self
            .checkpoint_expected_effect_fence
            .as_ref()
            .ok_or_else(|| err("terminal checkpoint participant has no bound Suspend effect"))?
            .operation_id
            .clone();
        let pending = CheckpointParticipant::pending(
            operation_id,
            request_fingerprint,
            snapshot_digest,
            snapshot_size_bytes,
        );
        begin_checkpoint_upload(
            &self.root,
            &self.lock,
            &mut self.marker,
            &authorization,
            pending,
        )
    }

    fn complete_checkpoint_upload(
        &mut self,
        request_fingerprint: &str,
        reference: &pc::SandboxCheckpointRef,
    ) -> Result<(), pc::SandboxError> {
        self.current_checkpoint_marker()?;
        let authorization = self.marker.effect_fence.clone();
        let operation_id = self
            .checkpoint_expected_effect_fence
            .as_ref()
            .ok_or_else(|| err("terminal checkpoint participant has no bound Suspend effect"))?
            .operation_id
            .clone();
        complete_checkpoint_upload(
            &self.root,
            &self.lock,
            &mut self.marker,
            &authorization,
            &operation_id,
            request_fingerprint,
            reference,
        )
    }

    fn validate_before_effect(&self) -> Result<(), pc::SandboxError> {
        self.current_checkpoint_marker().map(|_| ())
    }
}
