// @formal surface requirement=session.lifecycle.session_state
// @formal reason: The existing Session aggregate owns Environment admission, first intent and receipt
// @formal reason: transitions. Its caller commits the controlled mutation before provider I/O; admission
// @formal reason: and late receipt use the same materialization law. Original request closure and domain
// @formal reason: access remain physical evidence, so unknown Managed Rebuild stays pending, not proved.
use super::{
    CheckpointReceipt, QuiescenceReceipt, RestoreReceipt, SessionEnvironmentOperation,
    SessionEnvironmentOperations, SessionEnvironmentTransitionError, SourceDisposedReceipt,
};

impl SessionEnvironmentOperations<'_> {
    /// Admit the exact first value before provider I/O through the same root CAS.
    pub fn admit_environment_effect(
        &mut self,
        owner_scope: &str,
        intent: &crate::SessionEnvironmentEffectIntent,
        now_unix_ms: u64,
    ) -> Result<
        (bool, crate::SessionEnvironmentEffectAuthorization),
        crate::SessionEnvironmentReceiptError,
    > {
        let session = &mut self.aggregate.state;
        if session.is_terminal() {
            return Err(crate::SessionEnvironmentReceiptError::WrongPhase);
        }
        if let Some(facts) = session.environment_materialization.as_deref() {
            facts.validate_existing_origin_effect(intent)?;
        }
        let authorization =
            session.authorize_environment_effect(owner_scope, intent, now_unix_ms)?;
        match intent.kind() {
            crate::SessionEnvironmentEffectKind::Rebuild { .. } => {
                // Source closure/access is not available in this slice. A
                // target receipt or current lease cannot supply it.
                Err(crate::SessionEnvironmentReceiptError::MaterializationPending)
            }
            crate::SessionEnvironmentEffectKind::Create => {
                if session.realization.is_none() || intent.realization().is_none() {
                    return Err(crate::SessionEnvironmentReceiptError::RealizationStale);
                }
                let facts = session
                    .environment_materialization
                    .as_deref()
                    .ok_or(crate::SessionEnvironmentReceiptError::MaterializationUnknown)?;
                if let Some(first) = facts.pending().or_else(|| facts.origin()) {
                    if first.effect_id() != intent.effect_id() {
                        return Err(crate::SessionEnvironmentReceiptError::MaterializationPending);
                    }
                    // Checking the saved assertion also prevents a shortened
                    // candidate expiry from borrowing a grant below first.
                    let original =
                        session.authorize_environment_effect(owner_scope, first, now_unix_ms)?;
                    return Ok((false, original));
                }
                if !matches!(
                    authorization,
                    crate::SessionEnvironmentEffectAuthorization::Authorized { .. }
                ) {
                    return Err(crate::SessionEnvironmentReceiptError::InvalidTransition);
                }
                session
                    .environment_materialization
                    .as_deref_mut()
                    .expect("fresh materialization fact was checked")
                    .admit_first(intent);
                Ok((true, authorization))
            }
            crate::SessionEnvironmentEffectKind::Adopt
            | crate::SessionEnvironmentEffectKind::ResourceProjectionReservation { .. } => {
                Ok((false, authorization))
            }
        }
    }

    /// Apply the canonical PersistedSession::record_source_release_prepared law.
    pub fn record_source_release_prepared(
        &mut self,
        receipt: &crate::SourceReleasePreparedReceipt,
    ) -> Result<bool, crate::SessionEnvironmentReceiptError> {
        self.aggregate.state.record_source_release_prepared(receipt)
    }

    /// Apply the canonical PersistedSession::try_apply_environment_receipt law.
    pub fn try_apply_environment_receipt(
        &mut self,
        owner_scope: &str,
        receipt: &crate::SessionEnvironmentReceipt,
        now_unix_ms: u64,
    ) -> Result<bool, crate::SessionEnvironmentReceiptError> {
        let mut next = self.aggregate.clone();
        let changed =
            next.state
                .try_apply_environment_receipt(owner_scope, receipt, now_unix_ms)?;
        if changed {
            next.environment().install_binding_generation(now_unix_ms)?;
            *self.aggregate = next;
        }
        Ok(changed)
    }

    /// Apply the canonical SessionEnvironmentState::begin_suspend_at law.
    pub fn begin_suspend_at(
        &mut self,
        workspace_id: &str,
        started_at_unix_ms: u64,
    ) -> Result<&SessionEnvironmentOperation, SessionEnvironmentTransitionError> {
        if self.aggregate.state.is_terminal() || self.aggregate.state.has_active_activities() {
            return Err(SessionEnvironmentTransitionError::WrongPhase);
        }
        let activity_epoch = self.aggregate.state.activity_epoch;
        self.aggregate.state.environment.begin_suspend_at(
            workspace_id,
            &self.aggregate.state.session_id,
            activity_epoch,
            self.aggregate.state.realization.clone(),
            started_at_unix_ms,
        )
    }

    /// Apply the canonical SessionEnvironmentState::record_quiescence law.
    pub fn record_quiescence(
        &mut self,
        receipt: &QuiescenceReceipt,
        expected_mcp_generations: &[crate::McpGenerationRef],
    ) -> Result<bool, SessionEnvironmentTransitionError> {
        self.aggregate
            .state
            .environment
            .record_quiescence(receipt, expected_mcp_generations)
    }

    /// Apply the canonical SessionEnvironmentState::record_checkpoint law.
    pub fn record_checkpoint(
        &mut self,
        receipt: &CheckpointReceipt,
    ) -> Result<bool, SessionEnvironmentTransitionError> {
        self.aggregate.state.environment.record_checkpoint(receipt)
    }

    /// Apply the canonical SessionEnvironmentState::complete_suspend law.
    pub fn complete_suspend(
        &mut self,
        receipt: &SourceDisposedReceipt,
    ) -> Result<bool, SessionEnvironmentTransitionError> {
        self.aggregate.state.environment.complete_suspend(receipt)
    }

    /// Apply the canonical SessionEnvironmentState::begin_restore law.
    pub fn begin_restore(
        &mut self,
        workspace_id: &str,
        now_unix_ms: u64,
    ) -> Result<&SessionEnvironmentOperation, SessionEnvironmentTransitionError> {
        if self.aggregate.state.is_terminal() || self.aggregate.state.has_active_activities() {
            return Err(SessionEnvironmentTransitionError::WrongPhase);
        }
        let activity_epoch = self
            .aggregate
            .state
            .activity_epoch
            .checked_add(1)
            .ok_or(SessionEnvironmentTransitionError::WrongPhase)?;
        self.aggregate.state.environment.begin_restore(
            workspace_id,
            &self.aggregate.state.session_id,
            activity_epoch,
            self.aggregate.state.realization.clone(),
            now_unix_ms,
        )
    }

    /// Apply the canonical SessionEnvironmentState::complete_restore law.
    pub fn complete_restore(
        &mut self,
        receipt: &RestoreReceipt,
    ) -> Result<bool, SessionEnvironmentTransitionError> {
        self.aggregate.state.environment.complete_restore(receipt)
    }
}
