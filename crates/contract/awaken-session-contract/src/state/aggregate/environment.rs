// @formal surface requirement=session.lifecycle.session_state
// @formal reason: The existing Session aggregate owns Environment admission, first intent and receipt
// @formal reason: transitions. Its caller commits the controlled mutation before provider I/O; admission
// @formal reason: and late receipt use the same materialization law. Original request closure and domain
// @formal reason: access remain physical evidence, so unknown Managed Rebuild stays pending, not proved.
use super::{
    CheckpointReceipt, QuiescenceReceipt, RestoreReceipt, SessionEnvironmentDecision,
    SessionEnvironmentEvent, SessionEnvironmentFacts, SessionEnvironmentOperation,
    SessionEnvironmentOperations, SessionEnvironmentRejection, SessionEnvironmentStep,
    SessionEnvironmentTransitionError, SourceDisposedReceipt, SuspendPhase, transition_environment,
};

impl SessionEnvironmentOperations<'_> {
    /// Decide one Environment event over this candidate's projected facts.
    /// Every law below applies state only after this decision admitted it, so
    /// the terminality, lease and evidence rules hold on every path.
    fn decide(
        &self,
        event: SessionEnvironmentEvent,
        now_unix_ms: u64,
        asserted_lease: Option<&crate::state::SessionRealizationLease>,
    ) -> Result<SessionEnvironmentDecision, SessionEnvironmentRejection> {
        transition_environment(
            SessionEnvironmentFacts::project(&self.aggregate.state, now_unix_ms, asserted_lease),
            event,
        )
    }

    /// Decide a continuation command whose intent commits `intent`, the step
    /// after the current one. A replay of the committed intent returns its
    /// operation; any other decision refuses the intent.
    fn decide_intent(
        &self,
        event: SessionEnvironmentEvent,
        now_unix_ms: u64,
        intent: SessionEnvironmentStep,
    ) -> Result<(), SessionEnvironmentTransitionError> {
        if self.aggregate.state.environment.step() == intent {
            return Ok(());
        }
        let decision = transition_environment(
            SessionEnvironmentFacts::project_continuation(&self.aggregate.state, now_unix_ms),
            event,
        )?;
        match decision.instruction() {
            _ if decision.next() == intent => Ok(()),
            Some(crate::state::SessionEnvironmentInstruction::DeleteExpiredCheckpoint) => {
                Err(SessionEnvironmentTransitionError::CheckpointExpired)
            }
            _ => Err(SessionEnvironmentRejection::WrongStep.into()),
        }
    }

    /// The decided step must be the state the data law built.
    fn settled(&self, decision: SessionEnvironmentDecision) {
        debug_assert_eq!(
            self.aggregate.state.environment.step(),
            decision.next(),
            "an Environment law diverged from its decision"
        );
    }

    /// Admit the exact first value before provider I/O through the same root CAS.
    pub fn admit_environment_effect(
        &mut self,
        owner_scope: &str,
        intent: &crate::state::SessionEnvironmentEffectIntent,
        now_unix_ms: u64,
    ) -> Result<
        (bool, crate::state::SessionEnvironmentEffectAuthorization),
        crate::state::SessionEnvironmentReceiptError,
    > {
        let decided = self.decide(
            SessionEnvironmentEvent::AdmitMaterialization {
                kind: intent.kind().materialization_kind(),
                already_applied: self
                    .aggregate
                    .state
                    .environment
                    .records_effect(intent.effect_id()),
            },
            now_unix_ms,
            intent.realization(),
        );
        // A terminal Session refuses first. An unfinished original
        // materialization is retryable evidence that precedes every other
        // step refusal: the step may still become admissible.
        if decided != Err(SessionEnvironmentRejection::Terminal)
            && let Some(facts) = self.aggregate.state.environment_materialization.as_deref()
        {
            facts.validate_existing_origin_effect(intent)?;
        }
        decided?;
        let session = &mut self.aggregate.state;
        let authorization =
            session.authorize_environment_effect(owner_scope, intent, now_unix_ms)?;
        match intent.kind() {
            crate::state::SessionEnvironmentEffectKind::Rebuild { .. } => {
                // Source closure/access is not available in this slice. A
                // target receipt or current lease cannot supply it.
                Err(SessionEnvironmentRejection::MaterializationPending.into())
            }
            crate::state::SessionEnvironmentEffectKind::Create => {
                if session.realization.is_none() || intent.realization().is_none() {
                    return Err(crate::state::SessionEnvironmentReceiptError::RealizationStale);
                }
                let facts = session
                    .environment_materialization
                    .as_deref()
                    .ok_or(crate::state::SessionEnvironmentReceiptError::MaterializationUnknown)?;
                if let Some(first) = facts.pending().or_else(|| facts.origin()) {
                    if first.effect_id() != intent.effect_id() {
                        return Err(SessionEnvironmentRejection::MaterializationPending.into());
                    }
                    // Checking the saved assertion also prevents a shortened
                    // candidate expiry from borrowing a grant below first.
                    let original =
                        session.authorize_environment_effect(owner_scope, first, now_unix_ms)?;
                    return Ok((false, original));
                }
                if !matches!(
                    authorization,
                    crate::state::SessionEnvironmentEffectAuthorization::Authorized { .. }
                ) {
                    return Err(SessionEnvironmentRejection::InvalidTransition.into());
                }
                session
                    .environment_materialization
                    .as_deref_mut()
                    .expect("fresh materialization fact was checked")
                    .admit_first(intent);
                Ok((true, authorization))
            }
            crate::state::SessionEnvironmentEffectKind::Adopt
            | crate::state::SessionEnvironmentEffectKind::ResourceProjectionReservation {
                ..
            } => Ok((false, authorization)),
        }
    }

    /// Apply the canonical PersistedSession::record_source_release_prepared law.
    pub fn record_source_release_prepared(
        &mut self,
        receipt: &crate::state::SourceReleasePreparedReceipt,
        now_unix_ms: u64,
    ) -> Result<bool, crate::state::SessionEnvironmentReceiptError> {
        self.aggregate
            .state
            .record_source_release_prepared(receipt, now_unix_ms)
    }

    /// Apply the canonical PersistedSession::try_apply_environment_receipt law.
    pub fn try_apply_environment_receipt(
        &mut self,
        owner_scope: &str,
        receipt: &crate::state::SessionEnvironmentReceipt,
        now_unix_ms: u64,
    ) -> Result<bool, crate::state::SessionEnvironmentReceiptError> {
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

    /// Commit the suspend intent the advance decision demands (Resident to
    /// Quiescing), then apply the canonical begin_suspend_at law.
    pub fn begin_suspend_at(
        &mut self,
        workspace_id: &str,
        started_at_unix_ms: u64,
    ) -> Result<&SessionEnvironmentOperation, SessionEnvironmentTransitionError> {
        self.decide_intent(
            SessionEnvironmentEvent::AdvanceContinuation,
            started_at_unix_ms,
            SessionEnvironmentStep::Suspending(SuspendPhase::Quiescing),
        )?;
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
        expected_mcp_generations: &[crate::state::McpGenerationRef],
        now_unix_ms: u64,
    ) -> Result<bool, SessionEnvironmentTransitionError> {
        let decision = self.decide(SessionEnvironmentEvent::Quiesced, now_unix_ms, None)?;
        let changed = self
            .aggregate
            .state
            .environment
            .record_quiescence(receipt, expected_mcp_generations)?;
        self.settled(decision);
        Ok(changed)
    }

    /// Apply the canonical SessionEnvironmentState::record_checkpoint law.
    pub fn record_checkpoint(
        &mut self,
        receipt: &CheckpointReceipt,
        now_unix_ms: u64,
    ) -> Result<bool, SessionEnvironmentTransitionError> {
        let decision = self.decide(SessionEnvironmentEvent::Checkpointed, now_unix_ms, None)?;
        let changed = self
            .aggregate
            .state
            .environment
            .record_checkpoint(receipt)?;
        self.settled(decision);
        Ok(changed)
    }

    /// Apply the canonical SessionEnvironmentState::complete_suspend law.
    pub fn complete_suspend(
        &mut self,
        receipt: &SourceDisposedReceipt,
        now_unix_ms: u64,
    ) -> Result<bool, SessionEnvironmentTransitionError> {
        let decision = self.decide(SessionEnvironmentEvent::SourceReleased, now_unix_ms, None)?;
        let changed = self.aggregate.state.environment.complete_suspend(receipt)?;
        self.settled(decision);
        Ok(changed)
    }

    /// Commit the restore intent the require-resident decision demands
    /// (Hibernated to Restoring), then apply the canonical begin_restore law.
    pub fn begin_restore(
        &mut self,
        workspace_id: &str,
        now_unix_ms: u64,
    ) -> Result<&SessionEnvironmentOperation, SessionEnvironmentTransitionError> {
        self.decide_intent(
            SessionEnvironmentEvent::RequireResident,
            now_unix_ms,
            SessionEnvironmentStep::Restoring,
        )?;
        let activity_epoch = self
            .aggregate
            .state
            .activity_epoch
            .checked_add(1)
            .ok_or(SessionEnvironmentRejection::InvalidTransition)?;
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
        now_unix_ms: u64,
    ) -> Result<bool, SessionEnvironmentTransitionError> {
        let decision = self.decide(SessionEnvironmentEvent::Restored, now_unix_ms, None)?;
        let changed = self.aggregate.state.environment.complete_restore(receipt)?;
        self.settled(decision);
        Ok(changed)
    }
}
