// @formal surface requirement=runtime.capabilities.capability_floor
// @formal reason: Static owner: the terminal mutations of the held filesystem removal participant, split
// @formal reason: from the realization marker module without changing its authority. Dynamic path: every
// @formal reason: authority-advancing or destructive edge takes the admitted SandboxEffectGuard, rejects
// @formal reason: an effect other than the one admitted or older than the marker's recorded authority, and
// @formal reason: then performs the existing marker compare-and-swap under the held lifecycle lock; the
// @formal reason: guard-admission and terminal takeover decision tables qualify the behaviour, and this is
// @formal reason: not a direct formal-proof claim.
//! Terminal mutations of the one held filesystem removal participant. Each
//! destructive or authority-advancing edge takes the admitted effect guard and
//! then performs the marker compare-and-swap under the held lifecycle lock.
use super::*;

impl RemovalGuard {
    /// The durable authority this held marker currently records: the physical
    /// disposal authorization once persisted, else the realization fence.
    fn recorded_authority(&self) -> &pc::SandboxEffectFence {
        self.marker
            .disposal_authorization
            .as_ref()
            .map_or(&self.marker.effect_fence, |authorization| {
                authorization.effect_fence()
            })
    }

    /// Admission for a terminal mutation must be for `effect_fence` and must
    /// not be older than the lease the held marker already records. The marker
    /// compare-and-swap that follows is still the backend linearization point.
    fn require_admitted(
        &self,
        admitted: &pc::SandboxEffectGuard<'_>,
        effect_fence: &pc::SandboxEffectFence,
    ) -> Result<(), pc::SandboxError> {
        admitted.require_effect(effect_fence)?;
        admitted.require_not_superseded(self.recorded_authority())
    }

    /// Test fixture: admit this held participant under its recorded authority.
    #[cfg(test)]
    pub(crate) fn recorded_admission<T>(
        &mut self,
        apply: impl FnOnce(&mut Self, &pc::SandboxEffectGuard<'_>) -> Result<T, pc::SandboxError>,
    ) -> Result<T, pc::SandboxError> {
        let fence = self.recorded_authority().clone();
        pc::with_current_sandbox_effect(
            &fence,
            &pc::test_support::EffectAuthority::new(&fence),
            |admitted| apply(self, admitted),
        )
    }

    fn validate_disposal_authorization(&self) -> Result<(), pc::SandboxError> {
        let authorization = self.marker.disposal_authorization.as_ref().ok_or_else(|| {
            pc::SandboxError::fence_rejected(
                "filesystem physical mutation has no aggregate authorization authority",
            )
        })?;
        authorization.validate()
    }

    pub(crate) fn completed_receipt(
        &self,
    ) -> Result<Option<&RealizationCompletionReceipt>, pc::SandboxError> {
        if self.marker.phase == RealizationPhase::Removed {
            return Ok(self.marker.completion.as_ref());
        }
        if self.marker.terminal_source.is_some() && self.marker.completion.is_none() {
            return Err(pc::SandboxError::not_found(
                "completed filesystem realization entered terminal cleanup without its receipt",
            ));
        }
        Ok(self.marker.completion.as_ref())
    }

    pub(crate) fn refresh_authorization(
        &mut self,
        effect_fence: &pc::SandboxEffectFence,
        admitted: &pc::SandboxEffectGuard<'_>,
    ) -> Result<(), pc::SandboxError> {
        self.require_admitted(admitted, effect_fence)?;
        if self.marker.disposal_authorization.is_some() {
            return Err(pc::SandboxError::conflict(
                "filesystem realization already entered aggregate-authorized physical authorization",
            ));
        }
        effect_fence.validate_identity()?;
        if !self.marker.effect_fence.authorizes_successor(effect_fence) {
            return Err(pc::SandboxError::fence_rejected(
                "terminal disposal fence is stale, foreign, or regresses lease expiry",
            ));
        }
        if &self.marker.effect_fence != effect_fence {
            let mut next = self.marker.clone();
            next.effect_fence = effect_fence.clone();
            replace_marker_locked(&self.root, &self.lock, &self.marker, &next)?;
            self.marker = next;
        }
        Ok(())
    }

    /// Revalidate the one locked Removing participant immediately before an
    /// external reconstruction effect. Before aggregate authorization it reads
    /// (but never replaces) the durable prepared predecessor; after
    /// authorization it accepts only a structural successor of that physical-disposal
    /// authority. The exact marker and root identity are checked in both phases.
    pub(crate) fn validate_before_reconstruction(
        &self,
        effect_fence: &pc::SandboxEffectFence,
    ) -> Result<(), pc::SandboxError> {
        effect_fence.validate_identity()?;
        let authority = self
            .marker
            .disposal_authorization
            .as_ref()
            .map_or(&self.marker.effect_fence, |authorization| {
                authorization.effect_fence()
            });
        if !authority.authorizes_successor(effect_fence) {
            return Err(pc::SandboxError::unauthorized(
                "terminal reconstruction is not authorized by its durable predecessor",
            ));
        }
        let observed = read_marker_locked(&self.root, &self.lock)?.ok_or_else(|| {
            pc::SandboxError::gone("terminal reconstruction lost its realization marker")
        })?;
        if observed != self.marker || self.marker.phase != RealizationPhase::Removing {
            return Err(pc::SandboxError::conflict(
                "terminal reconstruction marker changed under its held lifecycle lock",
            ));
        }
        execution_participant::require_quiescent(&self.marker)?;
        require_exact_root_locked(
            &self.root,
            &self.lock,
            required_root_identity(&self.marker)?,
        )
    }

    /// Admit the provider-preparation boundary and return its exact durable
    /// predecessor. A same-operation renewal proves that the existing marker
    /// remains usable but does not replace it: if an earlier root CAS later
    /// wins, its receipt still names the marker exactly. A higher aggregate
    /// epoch starts a new provider predecessor; shorter, lower, or foreign
    /// requests have zero marker mutation.
    pub(crate) fn prepare_disposal_effect(
        &mut self,
        effect_fence: &pc::SandboxEffectFence,
        admitted: &pc::SandboxEffectGuard<'_>,
    ) -> Result<pc::SandboxEffectFence, pc::SandboxError> {
        self.require_admitted(admitted, effect_fence)?;
        execution_participant::require_quiescent(&self.marker)?;
        if self.marker.disposal_authorization.is_some() {
            return Err(pc::SandboxError::conflict(
                "filesystem realization already entered aggregate-authorized physical authorization",
            ));
        }
        effect_fence.validate_identity()?;

        if self
            .marker
            .effect_fence
            .same_realization_lease(effect_fence)
        {
            if !self
                .marker
                .effect_fence
                .authorizes_effect_successor(effect_fence)
            {
                return Err(pc::SandboxError::fence_rejected(
                    "provider preparation fence is shorter or belongs to another operation",
                ));
            }
            return Ok(self.marker.effect_fence.clone());
        }
        if !self
            .marker
            .effect_fence
            .authorizes_effect_successor(effect_fence)
        {
            return Err(pc::SandboxError::fence_rejected(
                "provider preparation fence is stale, foreign, or has a lower epoch",
            ));
        }

        let mut next = self.marker.clone();
        next.effect_fence = effect_fence.clone();
        replace_marker_locked(&self.root, &self.lock, &self.marker, &next)?;
        self.marker = next;
        Ok(effect_fence.clone())
    }

    /// Persist or advance the aggregate-derived physical authorization while
    /// retaining its immutable original preparation and fingerprint. The same
    /// marker CAS is the sole Local/Namespace gate: response-loss replay is
    /// exact, a newer authorized successor may take over, and an older or
    /// foreign successor cannot regain destructive authority.
    pub(crate) fn authorize_disposal(
        &mut self,
        authorization: &pc::SandboxDisposalAuthorization,
        admitted: &pc::SandboxEffectGuard<'_>,
    ) -> Result<(), pc::SandboxError> {
        self.require_admitted(admitted, authorization.effect_fence())?;
        execution_participant::require_quiescent(&self.marker)?;
        authorization.validate()?;

        let advance = match self.marker.disposal_authorization.as_ref() {
            None => {
                if &self.marker.effect_fence != authorization.prepared_effect_fence() {
                    return Err(pc::SandboxError::fence_rejected(
                        "physical disposal does not identify the exact prepared aggregate effect",
                    ));
                }
                true
            }
            Some(observed)
                if observed.prepared_effect_fence() != authorization.prepared_effect_fence()
                    || observed.preparation_fingerprint()
                        != authorization.preparation_fingerprint() =>
            {
                return Err(pc::SandboxError::conflict(
                    "physical disposal conflicts with the immutable aggregate preparation",
                ));
            }
            Some(observed) if observed.effect_fence() == authorization.effect_fence() => false,
            Some(observed)
                if observed
                    .effect_fence()
                    .authorizes_successor(authorization.effect_fence()) =>
            {
                true
            }
            Some(_) => {
                return Err(pc::SandboxError::fence_rejected(
                    "physical disposal successor is stale or conflicts with a newer takeover",
                ));
            }
        };
        if advance {
            let mut next = self.marker.clone();
            next.disposal_authorization = Some(authorization.clone());
            replace_marker_locked(&self.root, &self.lock, &self.marker, &next)?;
            self.marker = next;
        }
        Ok(())
    }

    pub(crate) fn bind_checkpoint_expected(
        &mut self,
        expected_effect_fence: &pc::SandboxEffectFence,
    ) -> Result<(), pc::SandboxError> {
        expected_effect_fence.validate_identity()?;
        let source = self.marker.terminal_source.as_ref().ok_or_else(|| {
            pc::SandboxError::not_found(
                "Removing checkpoint participant has no terminal source evidence",
            )
        })?;
        if source.expected_effect_fence.is_some()
            || source
                .realization_effect_fence
                .same_effect_identity(expected_effect_fence)
            || expected_effect_fence.same_effect_identity(&self.marker.effect_fence)
            || !source
                .realization_effect_fence
                .authorizes_successor(expected_effect_fence)
            || !expected_effect_fence.authorizes_successor(&self.marker.effect_fence)
        {
            return Err(pc::SandboxError::fence_rejected(
                "terminal checkpoint cleanup is not an authorized Suspend continuation",
            ));
        }
        if let Some(recorded) = &self.checkpoint_expected_effect_fence
            && !recorded.same_effect_identity(expected_effect_fence)
        {
            return Err(pc::SandboxError::conflict(
                "terminal checkpoint cleanup changed its expected source effect",
            ));
        }
        self.checkpoint_expected_effect_fence = Some(expected_effect_fence.clone());
        Ok(())
    }

    pub(super) fn current_checkpoint_marker(&self) -> Result<RealizationMarker, pc::SandboxError> {
        let expected = self
            .checkpoint_expected_effect_fence
            .as_ref()
            .ok_or_else(|| {
                pc::SandboxError::not_found(
                    "terminal checkpoint cleanup has no bound expected effect",
                )
            })?;
        self.marker.effect_fence.validate_identity()?;
        let marker = read_marker_locked(&self.root, &self.lock)?.ok_or_else(|| {
            pc::SandboxError::gone("terminal checkpoint cleanup lost its realization marker")
        })?;
        let source = marker.terminal_source.as_ref().ok_or_else(|| {
            pc::SandboxError::gone("terminal checkpoint cleanup lost its immutable source evidence")
        })?;
        if marker.phase != RealizationPhase::Removing
            || !marker
                .effect_fence
                .same_effect_identity(&self.marker.effect_fence)
            || source.expected_effect_fence.is_some()
            || !source
                .realization_effect_fence
                .authorizes_successor(expected)
            || !expected.authorizes_successor(&marker.effect_fence)
            || marker != self.marker
        {
            return Err(pc::SandboxError::conflict(
                "terminal checkpoint cleanup no longer owns the exact Removing participant",
            ));
        }
        owned_realization_path_locked(&self.root, &self.lock, &marker)?;
        execution_participant::require_quiescent(&marker)?;
        Ok(marker)
    }

    /// Return the exact currently-owned root or private stage. Providers use
    /// this only for pre-delete secret shredding; `None` proves that both names
    /// are absent under the held lifecycle lock.
    pub(crate) fn owned_root(
        &self,
    ) -> Result<Option<(PathBuf, DirectoryIdentity)>, pc::SandboxError> {
        execution_participant::require_quiescent(&self.marker)?;
        match owned_realization_path_locked(&self.root, &self.lock, &self.marker)? {
            OwnedRealizationPath::Absent => Ok(None),
            OwnedRealizationPath::Exact { path, identity } => Ok(Some((path, identity))),
        }
    }

    pub(crate) fn remove_root(
        &self,
        admitted: &pc::SandboxEffectGuard<'_>,
    ) -> Result<(), pc::SandboxError> {
        admitted.require_not_superseded(self.recorded_authority())?;
        if self.marker.phase == RealizationPhase::Removed {
            return require_realization_absent(&self.root, &self.lock, &self.marker);
        }
        self.validate_disposal_authorization()?;
        remove_owned_root_or_stage_locked(&self.root, &self.lock, &self.marker)
    }

    pub(crate) fn finish(
        &mut self,
        admitted: &pc::SandboxEffectGuard<'_>,
    ) -> Result<(), pc::SandboxError> {
        admitted.require_not_superseded(self.recorded_authority())?;
        require_realization_absent(&self.root, &self.lock, &self.marker)?;
        if self.marker.phase == RealizationPhase::Removed {
            return Ok(());
        }
        self.validate_disposal_authorization()?;
        let mut next = self.marker.clone();
        next.phase = RealizationPhase::Removed;
        // A failed directory sync/response after rename may have published the
        // exact intended tombstone. Reconcile only that complete value under
        // the original lock, and re-run durable publication before reporting
        // success. Foreign marker content is never accepted as completion.
        let observed = read_marker_locked(&self.root, &self.lock)?;
        let expected = if observed.as_ref() == Some(&next) {
            &next
        } else {
            &self.marker
        };
        replace_marker_locked(&self.root, &self.lock, expected, &next)?;
        self.marker = next;
        Ok(())
    }
}
