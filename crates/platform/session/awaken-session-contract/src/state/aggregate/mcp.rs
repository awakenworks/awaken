// @formal surface requirement=session.lifecycle.session_state
// @formal reason: MCP claim, recovery and replacement delegate to the canonical attachment-set laws
// @formal reason: within one Session candidate, retaining generation and plaintext-holder boundaries.
// @formal reason: This requirement link does not prove external attachment delivery or secret custody.
use super::{
    McpAttachmentDraft, McpAttachmentError, McpGeneration, McpGenerationRef, McpRealizationClaim,
    McpReplacementPlan, PlaintextHolder, SessionMcpOperations,
};

impl super::SessionAggregate {
    /// A claimed preparation may replace an exact credential while its one
    /// named Run is logically active, before that Run acquires a physical slot.
    /// The caller separately retains the Dispatch exclusion. Complete original
    /// activity membership is required; a count, a legacy empty set, or a parent
    /// Session identity cannot stand in for the current Run's coordinates.
    pub fn admits_claimed_mcp_credential_preparation(
        &self,
        thread: &awaken_agent_contract::ThreadId,
        run: &awaken_agent_contract::RunId,
    ) -> bool {
        if self.state.is_terminal() || self.state.active_activity_epochs.len() != 1 {
            return false;
        }
        let Some((records, false)) = self.state.run_activities() else {
            return false;
        };
        let epoch = *self
            .state
            .active_activity_epochs
            .first()
            .expect("one active epoch");
        records
            .iter()
            .filter(|record| record.activity_epoch().get() == epoch)
            .count()
            == 1
            && records.iter().any(|record| {
                record.activity_epoch().get() == epoch
                    && record.thread_id() == thread
                    && record.run_id() == run
            })
            && matches!(
                self.state.execution,
                super::SessionExecutionState::Preparing
                    | super::SessionExecutionState::Activating
                    | super::SessionExecutionState::Running
            )
    }
}

impl SessionMcpOperations<'_> {
    /// Apply the canonical SessionMcpAttachmentSet::claim_realization law.
    pub fn claim_realization(
        &mut self,
        generation: &McpGeneration,
        claim: McpRealizationClaim,
    ) -> Result<(), McpAttachmentError> {
        if self.aggregate.state.is_terminal() {
            return Err(McpAttachmentError::InvalidTransition);
        }
        self.aggregate
            .state
            .mcp
            .claim_realization(generation, claim)
    }

    /// Apply the canonical SessionMcpAttachmentSet::claim_recovery law.
    pub fn claim_recovery(
        &mut self,
        generation: &McpGeneration,
        claim: McpRealizationClaim,
    ) -> Result<(), McpAttachmentError> {
        if self.aggregate.state.is_terminal() {
            return Err(McpAttachmentError::InvalidTransition);
        }
        self.aggregate.state.mcp.claim_recovery(generation, claim)
    }

    /// Apply the canonical SessionMcpAttachmentSet::request_full_replacement law.
    pub fn request_full_replacement(
        &mut self,
        drafts: Vec<McpAttachmentDraft>,
        selected_holder: Option<PlaintextHolder>,
    ) -> Result<McpReplacementPlan, McpAttachmentError> {
        if self.aggregate.state.is_terminal() {
            return Err(McpAttachmentError::InvalidTransition);
        }
        self.aggregate
            .state
            .mcp
            .request_full_replacement(drafts, selected_holder)
    }

    /// Apply the canonical SessionMcpAttachmentSet::begin_obsolete_drains law.
    pub fn begin_obsolete_drains(&mut self) -> Result<(), McpAttachmentError> {
        self.aggregate.state.mcp.begin_obsolete_drains()
    }

    /// Apply the canonical SessionMcpAttachmentSet::activate law.
    pub fn activate(
        &mut self,
        generation: &McpGeneration,
        realization_id: &str,
    ) -> Result<(), McpAttachmentError> {
        if self.aggregate.state.is_terminal() {
            return Err(McpAttachmentError::InvalidTransition);
        }
        self.aggregate
            .state
            .mcp
            .activate(generation, realization_id)
    }

    /// Apply the canonical SessionMcpAttachmentSet::acknowledge_publication law.
    pub fn acknowledge_publication(
        &mut self,
        generation: &McpGeneration,
        realization_id: &str,
    ) -> Result<(), McpAttachmentError> {
        if self.aggregate.state.is_terminal() {
            return Err(McpAttachmentError::InvalidTransition);
        }
        self.aggregate
            .state
            .mcp
            .acknowledge_publication(generation, realization_id)
    }

    /// Apply the canonical SessionMcpAttachmentSet::fail_realization law.
    pub fn fail_realization(
        &mut self,
        generation: &McpGeneration,
        realization_id: &str,
        error: impl Into<String>,
    ) -> Result<(), McpAttachmentError> {
        self.aggregate
            .state
            .mcp
            .fail_realization(generation, realization_id, error)
    }

    /// Apply the canonical SessionMcpAttachmentSet::fail_unpublished_generations law.
    pub fn fail_unpublished_generations(
        &mut self,
        error: impl Into<String>,
    ) -> Result<usize, McpAttachmentError> {
        self.aggregate.state.mcp.fail_unpublished_generations(error)
    }

    /// Apply the canonical SessionMcpAttachmentSet::begin_drain law.
    pub fn begin_drain(&mut self, generation: &McpGeneration) -> Result<(), McpAttachmentError> {
        self.aggregate.state.mcp.begin_drain(generation)
    }

    /// Apply the canonical SessionMcpAttachmentSet::finish_drain law.
    pub fn finish_drain(&mut self, generation: &McpGeneration) -> Result<(), McpAttachmentError> {
        self.aggregate.state.mcp.finish_drain(generation)
    }

    /// Apply the canonical SessionMcpAttachmentSet::require_reprojection_after_quiescence law.
    pub fn require_reprojection_after_quiescence(
        &mut self,
        quiesced: &[McpGenerationRef],
    ) -> Result<usize, McpAttachmentError> {
        self.aggregate
            .state
            .mcp
            .require_reprojection_after_quiescence(&self.aggregate.state.session_id, quiesced)
    }
}
