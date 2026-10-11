use super::{
    CheckpointReceipt, QuiescenceReceipt, RestoreReceipt, SessionEnvironmentOperation,
    SessionEnvironmentOperations, SessionEnvironmentTransitionError, SourceDisposedReceipt,
};

impl SessionEnvironmentOperations<'_> {
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
