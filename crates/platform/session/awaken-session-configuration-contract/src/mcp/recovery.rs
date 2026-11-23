// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: The existing Session MCP aggregate owns recovery claims and preparation selection.
// @formal reason: Both consume its one viable same-source credential-retirement predicate, retaining
// @formal reason: physical cleanup evidence and ordinary/fallback Stage requirements. No new lifecycle,
// @formal reason: authority or wire state is added. Application/core cause-effect tests verify the
// @formal reason: hot-lease gap and receipt fences; this is classification, not a direct formal proof.
//! Shared recovery and preparation law of the canonical MCP attachment set.

use super::{
    McpAttachmentError, McpAttachmentState, McpGeneration, McpRealizationClaim,
    SessionMcpAttachment, SessionMcpAttachmentSet, exact_index, exact_mut,
};

impl SessionMcpAttachmentSet {
    /// Claimed effects still requiring preparation. A known retired credential
    /// cannot block its viable successor even when the same live lease did not
    /// invoke claim_recovery again. Ordinary replacement and failed-successor
    /// fallback retain their original required Stage receipts.
    #[must_use]
    pub fn staging_attachments(&self) -> Vec<&SessionMcpAttachment> {
        let desired = self.desired_attachments();
        self.attachments
            .iter()
            .filter(|attachment| {
                let pending = attachment.state == McpAttachmentState::Realizing
                    || (attachment.state == McpAttachmentState::Active
                        && !attachment.publication_acknowledged);
                pending
                    && !Self::credential_predecessor_can_drain(
                        attachment,
                        desired
                            .iter()
                            .copied()
                            .find(|head| head.name == attachment.name),
                    )
            })
            .collect()
    }

    fn credential_predecessor_can_drain(
        current: &SessionMcpAttachment,
        desired: Option<&SessionMcpAttachment>,
    ) -> bool {
        current.realization.is_some()
            && desired.is_some_and(|desired| {
                matches!(
                    desired.state,
                    McpAttachmentState::Requested
                        | McpAttachmentState::Realizing
                        | McpAttachmentState::Active
                ) && desired.generation != current.generation
                    && current
                        .credential
                        .as_ref()
                        .zip(desired.credential.as_ref())
                        .is_some_and(|(old, new)| {
                            old.credential.id == new.credential.id
                                && new.credential.revision > old.credential.revision
                        })
            })
    }

    /// Fence crash recovery to a new Session lease before any Runtime effect.
    /// Requested generations enter Realizing; already Realizing or Active
    /// generations retain their durable visibility while replacing only the
    /// expired process-local realization claim. Only a viable same-source
    /// credential advance retires its predecessor to the existing drain path;
    /// a Failed successor cannot supersede it.
    pub fn claim_recovery(
        &mut self,
        generation: &McpGeneration,
        claim: McpRealizationClaim,
    ) -> Result<(), McpAttachmentError> {
        let index = exact_index(&self.attachments, generation)?;
        let current = &self.attachments[index];
        // Only a typed advance of this credential source makes reopening its
        // predecessor impossible. Ordinary target/name replacement keeps the
        // existing recovery and receipt-validation contract. This does not
        // authorize an advance: the Session's original credential owner has
        // already admitted the successor before it enters the desired set.
        let retired_credential = Self::credential_predecessor_can_drain(
            current,
            self.desired_attachments()
                .into_iter()
                .find(|desired| desired.name == current.name),
        );
        if retired_credential
            && matches!(
                current.state,
                McpAttachmentState::Realizing | McpAttachmentState::Active
            )
        {
            self.attachments[index].state = McpAttachmentState::Draining;
            return self.bump_revision();
        }
        let attachment = exact_mut(&mut self.attachments, generation)?;
        match attachment.state {
            McpAttachmentState::Requested => {
                attachment.state = McpAttachmentState::Realizing;
            }
            McpAttachmentState::Realizing | McpAttachmentState::Active => {}
            McpAttachmentState::Draining
            | McpAttachmentState::Removed
            | McpAttachmentState::Failed => return Err(McpAttachmentError::InvalidTransition),
        }
        attachment.attempts = attachment
            .attempts
            .checked_add(1)
            .ok_or(McpAttachmentError::CounterExhausted)?;
        attachment.realization = Some(claim);
        attachment.publication_acknowledged = false;
        self.bump_revision()
    }
}
