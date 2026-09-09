//! One post-activation publication plan for the durable Session generation set.
use super::{generation_set_authorizes, projection, unavailable};
use awaken_session_contract::{
    McpAttachmentState, McpGenerationRef, SessionMcpAttachment, SessionRealizationAction,
    SessionRealizationControlFailure,
};

pub(super) fn publication_generations(
    session_id: &str,
    attachments: &[SessionMcpAttachment],
) -> Result<Vec<McpGenerationRef>, SessionRealizationControlFailure> {
    attachments
        .iter()
        .filter(|attachment| {
            attachment.state == McpAttachmentState::Active && !attachment.publication_acknowledged
        })
        .map(|attachment| {
            projection::mcp_generation_ref(session_id, attachment).map_err(unavailable)
        })
        .collect()
}

pub(super) fn draining_generations(
    session_id: &str,
    attachments: &[SessionMcpAttachment],
) -> Result<Vec<McpGenerationRef>, SessionRealizationControlFailure> {
    attachments
        .iter()
        .filter(|attachment| attachment.state == McpAttachmentState::Draining)
        .map(|attachment| {
            projection::mcp_generation_ref(session_id, attachment).map_err(unavailable)
        })
        .collect()
}

/// Select complete physical receipts against the post-commit root. Retired
/// Stage receipts remain drain authority and cannot become publication work.
/// The exact existing bijection rejects missing or duplicated live receipts.
pub(super) fn publication_action(
    session_id: &str,
    attachments: &[SessionMcpAttachment],
    receipts: Vec<McpGenerationRef>,
) -> Result<SessionRealizationAction, SessionRealizationControlFailure> {
    let expected_publish = publication_generations(session_id, attachments)?;
    let publish = receipts
        .into_iter()
        .filter(|receipt| {
            expected_publish.iter().any(|current| {
                awaken_session_contract::realization_generation_authorizes(current, receipt)
            })
        })
        .collect::<Vec<_>>();
    if !generation_set_authorizes(&expected_publish, &publish) {
        return Err(SessionRealizationControlFailure::Invalid(
            "activated publication set has no exact staged receipt set".into(),
        ));
    }
    let drain = draining_generations(session_id, attachments)?;
    Ok(SessionRealizationAction::Publish { publish, drain })
}

#[cfg(test)]
mod collection_tests;
