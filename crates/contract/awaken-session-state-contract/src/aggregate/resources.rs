// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Resource preparation, activation, rollback and retirement use the canonical resource
// @formal reason: state within one Session candidate. Original root CAS and physical receipt checks
// @formal reason: remain required; this classification does not prove external Resource availability.
use super::{
    ResolvedInput, ResolvedSessionResources, ResourceActivationError, SessionResourcesOperations,
};

impl SessionResourcesOperations<'_> {
    /// Apply the canonical SessionResourceState::compact_settled_activations law.
    pub fn compact_settled_activations(&mut self) -> bool {
        self.aggregate.state.resources.compact_settled_activations()
    }

    /// Apply the canonical SessionResourceState::prepare law.
    pub fn prepare(
        &mut self,
        desired: ResolvedSessionResources,
    ) -> Result<u64, ResourceActivationError> {
        if self.aggregate.state.is_terminal() {
            return Err(ResourceActivationError::Terminal);
        }
        self.aggregate
            .state
            .resources
            .prepare(&self.aggregate.state.session_id, desired)
    }

    /// Apply the canonical SessionResourceState::start_attempt law.
    pub fn start_attempt(&mut self) -> Result<(), ResourceActivationError> {
        if self.aggregate.state.is_terminal() {
            return Err(ResourceActivationError::Terminal);
        }
        self.aggregate.state.resources.start_attempt()
    }

    /// Apply the canonical SessionResourceState::revise_unattempted_pending law.
    pub fn revise_unattempted_pending(
        &mut self,
        desired: ResolvedSessionResources,
    ) -> Result<u64, ResourceActivationError> {
        if self.aggregate.state.is_terminal() {
            return Err(ResourceActivationError::Terminal);
        }
        self.aggregate
            .state
            .resources
            .revise_unattempted_pending(&self.aggregate.state.session_id, desired)
    }

    /// Apply the canonical SessionResourceState::commit law.
    pub fn commit(&mut self) -> Result<(), ResourceActivationError> {
        if self.aggregate.state.is_terminal() {
            return Err(ResourceActivationError::Terminal);
        }
        self.aggregate.state.resources.commit()
    }

    /// Apply the canonical SessionResourceState::ensure_terminal_repository_retirements law.
    pub fn ensure_terminal_repository_retirements(&mut self) -> bool {
        self.aggregate
            .state
            .resources
            .ensure_terminal_repository_retirements()
    }

    /// Apply the canonical SessionResourceState::complete_repository_retirement law.
    pub fn complete_repository_retirement(&mut self, completed: &ResolvedInput) -> bool {
        self.aggregate
            .state
            .resources
            .complete_repository_retirement(completed)
    }

    /// Apply the canonical SessionResourceState::note_retryable_failure law.
    pub fn note_retryable_failure(
        &mut self,
        error: impl Into<String>,
    ) -> Result<(), ResourceActivationError> {
        self.aggregate.state.resources.note_retryable_failure(error)
    }

    /// Apply the canonical SessionResourceState::rollback law.
    pub fn rollback(&mut self, error: impl Into<String>) -> Result<(), ResourceActivationError> {
        self.aggregate.state.resources.rollback(error)
    }

    /// Apply the canonical SessionResourceState::adopt_legacy_active law.
    pub fn adopt_legacy_active(&mut self) {
        self.aggregate
            .state
            .resources
            .adopt_legacy_active(&self.aggregate.state.session_id)
    }

    /// Apply the canonical SessionResourceState::begin_release law.
    pub fn begin_release(&mut self) -> Result<(), ResourceActivationError> {
        self.aggregate.state.resources.begin_release()
    }

    /// Apply the canonical SessionResourceState::complete_release law.
    pub fn complete_release(&mut self) {
        self.aggregate.state.resources.complete_release()
    }

    /// Apply the canonical SessionResourceState::complete_terminal_release law.
    pub fn complete_terminal_release(&mut self, reason: impl Into<String>) {
        self.aggregate
            .state
            .resources
            .complete_terminal_release(reason)
    }
}
