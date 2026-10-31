// @formal entrypoint sandbox.scoped_launch_requires_current_grant_and_init_exit_for_quiescence
//! Process participation in the one filesystem realization lifecycle.
//! No persisted PID is ever used as signal authority. The same lifecycle lock
//! serializes pre-GO publication against cold adoption, snapshot and deletion.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;
#[cfg(target_os = "linux")]
use awaken_local_process::ProcessExecutionIdentity;

const MAX_EXECUTIONS: usize = 128;

pub(super) fn validate(marker: &RealizationMarker) -> Result<(), pc::SandboxError> {
    for (phase, executions) in std::iter::once((marker.phase, &marker.executions)).chain(
        marker
            .rebuild_source
            .iter()
            .map(|source| (source.phase, &source.executions)),
    ) {
        if executions.len() > MAX_EXECUTIONS
            || (!executions.is_empty()
                && !matches!(
                    phase,
                    RealizationPhase::Ready
                        | RealizationPhase::Removing
                        | RealizationPhase::Removed
                ))
        {
            return Err(pc::SandboxError::invalid(
                "invalid execution participant phase or capacity",
            ));
        }
        for (index, identity) in executions.iter().enumerate() {
            identity
                .validate()
                .map_err(|error| pc::SandboxError::backend("validate", error))?;
            if executions[..index].contains(identity) {
                return Err(pc::SandboxError::invalid(
                    "duplicate execution participant identity",
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn is_quiescent(marker: &RealizationMarker) -> Result<bool, pc::SandboxError> {
    for identity in marker.executions.iter().chain(
        marker
            .rebuild_source
            .iter()
            .flat_map(|source| source.executions.iter()),
    ) {
        if !identity
            .is_quiescent()
            .map_err(|error| pc::SandboxError::backend("is_quiescent", error))?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) fn require_quiescent(marker: &RealizationMarker) -> Result<(), pc::SandboxError> {
    if is_quiescent(marker)? {
        Ok(())
    } else {
        Err(pc::SandboxError::not_ready(
            "filesystem realization still has a live execution participant",
        ))
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn begin_execution(
    root: &Path,
    evidence: &RealizationEvidence,
) -> Result<ReadyOperationGuard, pc::SandboxError> {
    // The creation fence identifies this physical incarnation, but its old
    // expiry must NOT substitute for CURRENT Session authority at GO. This is
    // read-only admission under the same guard used by other Ready operations.
    let guard = begin_ready_operation_with_authorization(
        root,
        evidence,
        evidence.effect_fence(),
        evidence.effect_fence(),
    )?;
    if guard.marker.checkpoint.is_some() {
        return Err(pc::SandboxError::gone(
            "checkpoint participant has closed execution admission",
        ));
    }
    Ok(guard)
}

#[cfg(target_os = "linux")]
pub(crate) fn verify_execution_quiescence(
    root: &Path,
    evidence: &RealizationEvidence,
) -> Result<(), pc::SandboxError> {
    let lock = acquire(root)?;
    let marker = read_marker_locked(root, &lock)?.ok_or_else(|| {
        pc::SandboxError::gone("execution quiescence lost its realization marker")
    })?;
    observation::validate_handle_marker(
        &marker,
        evidence.fingerprint(),
        evidence.effect_fence(),
        evidence.physical_incarnation(),
    )?;
    if required_root_identity(&marker)? != evidence.root_identity() {
        return Err(pc::SandboxError::conflict(
            "execution quiescence changed its physical root evidence",
        ));
    }
    require_quiescent(&marker)
}

#[cfg(target_os = "linux")]
impl RemovalGuard {
    pub(crate) fn verify_execution_quiescence(&self) -> Result<(), pc::SandboxError> {
        let observed = read_marker_locked(&self.root, &self.lock)?;
        if observed.as_ref() != Some(&self.marker) {
            return Err(pc::SandboxError::conflict(
                "execution quiescence changed its retained removal participant",
            ));
        }
        require_quiescent(&self.marker)
    }
}

#[cfg(target_os = "linux")]
impl ReadyOperationGuard {
    /// Invoke only inside current Session authorization, immediately before GO.
    /// A WouldBlock retry is idempotent. A failed GO can leave a participant;
    /// that is deliberately recovered by OS observation, never erased on error.
    pub(crate) fn record_execution(
        &mut self,
        identity: &ProcessExecutionIdentity,
    ) -> Result<(), pc::SandboxError> {
        identity
            .validate()
            .map_err(|error| pc::SandboxError::backend("record_execution", error))?;
        self.current_marker()?;
        if self.marker.checkpoint.is_some() {
            return Err(pc::SandboxError::gone(
                "checkpoint participant has closed execution admission",
            ));
        }
        if self.marker.executions.contains(identity) {
            return Ok(());
        }
        let mut next = self.marker.clone();
        // Reclaim only positively absent records. Unknown participants keep
        // their evidence and conservatively block a new launch as well.
        let mut retained = Vec::new();
        for previous in &next.executions {
            if !previous
                .is_quiescent()
                .map_err(|error| pc::SandboxError::backend("record_execution", error))?
            {
                retained.push(previous.clone());
            }
        }
        if retained.len() >= MAX_EXECUTIONS {
            return Err(pc::SandboxError::not_ready(
                "execution participant capacity reached",
            ));
        }
        retained.push(identity.clone());
        next.executions = retained;
        replace_marker_locked(&self.root, &self.lock, &self.marker, &next)?;
        self.marker = next;
        Ok(())
    }
}
