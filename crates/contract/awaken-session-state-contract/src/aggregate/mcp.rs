use super::{
    McpAttachmentDraft, McpAttachmentError, McpGeneration, McpGenerationRef, McpRealizationClaim,
    McpReplacementPlan, PlaintextHolder, SessionMcpOperations,
};

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
