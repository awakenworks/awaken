// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Checkpoint expiry and binding-generation updates stay within the existing Session
// @formal reason: candidate and its receipt/generation laws; the application commits root CAS before
// @formal reason: observing success. This link is not proof of provider deletion or materialization.
use super::{
    SandboxGeneration, SessionEnvironmentOperations, SessionEnvironmentReceiptError,
    SessionEnvironmentState, SessionEnvironmentTransitionError,
};

impl SessionEnvironmentOperations<'_> {
    /// Discard only an expired retained checkpoint after its external deletion
    /// succeeded: the decision admits the deletion receipt only for an active
    /// Hibernated Environment whose checkpoint or generation expired. The
    /// subsequent root CAS fences a concurrent newer generation.
    pub fn discard_expired_checkpoint(
        &mut self,
        now: u64,
    ) -> Result<bool, SessionEnvironmentTransitionError> {
        // The only admitted edge is Hibernated to Unmaterialized.
        crate::state::transition_environment(
            crate::state::SessionEnvironmentFacts::project(&self.aggregate.state, now, None),
            crate::state::SessionEnvironmentEvent::ExpiredCheckpointDeleted,
        )?;
        self.aggregate.state.environment = SessionEnvironmentState::Unmaterialized;
        Ok(true)
    }

    /// Derive a missing binding generation from this root's immutable baseline,
    /// never a caller-supplied environment or image fingerprint.
    pub(super) fn install_binding_generation(
        &mut self,
        now: u64,
    ) -> Result<(), SessionEnvironmentReceiptError> {
        let state = &mut self.aggregate.state;
        if state.environment.generation().is_some() {
            return Ok(());
        }
        let baseline = state
            .frozen_baseline()
            .ok_or(SessionEnvironmentReceiptError::EnvironmentMismatch)?;
        let retention_ms = baseline
            .environment
            .idle_retention
            .retention_secs
            .saturating_mul(1_000);
        let expires = if retention_ms == 0 {
            u64::MAX
        } else {
            now.saturating_add(retention_ms)
        };
        let environment = baseline.environment.config_fingerprint.0.clone();
        let image = baseline
            .environment
            .prepared_image
            .clone()
            .unwrap_or_else(|| crate::state::stable_fingerprint(&baseline.environment.sandbox));
        state.environment.assign_generation(SandboxGeneration::new(
            &state.session_id,
            now,
            expires,
            environment,
            image,
        ));
        Ok(())
    }
}
