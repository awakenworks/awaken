//! One post-activation publication plan for the durable Session generation set.
use super::{generation_set_authorizes, projection, unavailable};
use awaken_session_contract::{
    McpAttachmentState, McpGenerationRef, SessionMcpAttachment, SessionRealizationAction,
    SessionRealizationControlFailure,
};

/// The one publication-membership predicate used by every realization caller.
/// Stage and retirement remain MCP aggregate transitions; publication selects
/// only an unacknowledged Active generation.
const fn realization_publication_selects(
    state: McpAttachmentState,
    publication_acknowledged: bool,
) -> bool {
    matches!(state, McpAttachmentState::Active) && !publication_acknowledged
}

pub(super) fn publication_generations(
    session_id: &awaken_session_contract::SessionId,
    attachments: &[SessionMcpAttachment],
) -> Result<Vec<McpGenerationRef>, SessionRealizationControlFailure> {
    attachments
        .iter()
        .filter(|attachment| {
            realization_publication_selects(attachment.state, attachment.publication_acknowledged)
        })
        .map(|attachment| {
            projection::mcp_generation_ref(session_id, attachment).map_err(unavailable)
        })
        .collect()
}

pub(super) fn draining_generations(
    session_id: &awaken_session_contract::SessionId,
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
    let expected_publish = publication_generations(
        &awaken_session_contract::SessionId::from(session_id),
        attachments,
    )?;
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
    let drain = draining_generations(
        &awaken_session_contract::SessionId::from(session_id),
        attachments,
    )?;
    Ok(SessionRealizationAction::Publish { publish, drain })
}

#[cfg(kani)]
#[kani::proof]
fn realization_publication_selects_only_unacknowledged_active_generation() {
    // Cause/effect decision table: C1 state is Requested, Realizing, Active,
    // Draining, Removed, or Failed; C2 publication is acknowledged. R1 selects
    // iff C1=Active and C2=false; every staged, retired, terminal, or already
    // acknowledged row is excluded. Kani covers all 12 state/acknowledgement
    // rows against the exact production predicate consumed above.
    let discriminator: u8 = kani::any();
    kani::assume(discriminator < 6);
    let state = match discriminator {
        0 => McpAttachmentState::Requested,
        1 => McpAttachmentState::Realizing,
        2 => McpAttachmentState::Active,
        3 => McpAttachmentState::Draining,
        4 => McpAttachmentState::Removed,
        _ => McpAttachmentState::Failed,
    };
    let acknowledged: bool = kani::any();
    assert_eq!(
        realization_publication_selects(state, acknowledged),
        state == McpAttachmentState::Active && !acknowledged
    );
}

#[cfg(test)]
mod collection_tests;
