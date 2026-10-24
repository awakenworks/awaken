//! The recording cleanup runtime's terminal-cleanup role: publication, preparation and disposal effects in order.
use super::*;

#[async_trait::async_trait]
impl awaken_session_execution_contract::TerminalCleanupRuntime for RecordingCleanupRuntime {
    async fn observe_terminal_cleanup_retention(
        &self,
        session_id: &awaken_session_execution_contract::__macro_support::SessionId,
        projection: &awaken_session_execution_contract::__macro_support::FrozenSessionProjection,
        source_prepared: bool,
    ) -> Result<bool, awaken_session_execution_contract::__macro_support::RunError> {
        self.retention_reads.lock().unwrap().push((
            session_id.to_string(),
            projection.clone(),
            source_prepared,
        ));
        if self.retention_unavailable.load(Ordering::SeqCst) {
            return Err(RunError::unavailable(
                "retained source observation unavailable",
            ));
        }
        Ok(self.source_retained.load(Ordering::SeqCst))
    }

    async fn prepare_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_state_contract::SessionTerminalCleanupEffect,
        authorization: awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization,
    ) -> Result<awaken_session_state_contract::SessionCleanupPreparation, RunError> {
        authorization
            .verify_for(&effect)
            .map_err(|error| RunError::internal(error.to_string()))?;
        let assignment = self
            .terminal_assignments
            .lock()
            .unwrap()
            .last()
            .cloned()
            .ok_or_else(|| RunError::internal("terminal effect has no installed assignment"))?;
        if assignment.session_id != effect.command.session_id
            || assignment.authority.lease != effect.lease
        {
            return Err(RunError::internal(
                "terminal effect does not match its installed assignment",
            ));
        }
        let intent = effect.command.clone();
        if self.fail_before_effect_once.swap(false, Ordering::SeqCst) {
            return Err(RunError::internal("injected pre-effect crash window"));
        }
        self.intents.lock().unwrap().push(intent.clone());
        self.terminal_effect_order
            .lock()
            .unwrap()
            .push(format!("cleanup:{}", intent.thread_id));
        self.effective_ids
            .lock()
            .unwrap()
            .insert(intent.effect_id.clone());
        if self.fail_once.swap(false, Ordering::SeqCst) {
            return Err(RunError::internal("injected cleanup crash window"));
        }
        let provider_prepared_effect_fence = effect
            .sandbox_effect_fence()
            .map_err(|error| RunError::internal(error.to_string()))?;
        awaken_session_state_contract::SessionCleanupPreparation::try_new(
            &effect,
            provider_prepared_effect_fence,
            Vec::new(),
        )
        .map_err(|error| RunError::internal(error.to_string()))
    }

    async fn acknowledge_terminal_cleanup_preparation(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
    ) {
        self.acknowledged_effects
            .lock()
            .unwrap()
            .push(effect.clone());
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Retire only the process-local teardown projection after Control has
    /// durably accepted the exact disposal receipt. No provider or Resource
    /// effect is permitted at this edge.
    async fn acknowledge_terminal_cleanup_disposal(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) {
        awaken_session_execution_contract::NeutralRuntime
            .acknowledge_terminal_cleanup_disposal(effect)
            .await
    }

    async fn acknowledge_completed_terminal_cleanup(
        &self,
        session_id: &SessionId,
        lease: &crate::session_resource::SessionRealizationLease,
    ) {
        self.acknowledged_completed
            .lock()
            .unwrap()
            .push((session_id.to_string(), lease.clone()));
    }

    async fn install_terminal_cleanup_assignment(
        &self,
        assignment: &awaken_session_realization_contract::SessionTerminalCleanupAssignment,
        _proof: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), RunError> {
        self.terminal_assignments
            .lock()
            .unwrap()
            .push(assignment.clone());
        Ok(())
    }
}
