// @formal entrypoint session_realization.publication_excludes_staged_retired_generation
//! One post-activation publication plan for the durable Session generation set.
use super::{generation_set_authorizes, projection, unavailable};
use awaken_session_configuration_contract::mcp::{
    McpAttachmentState, McpGenerationRef, SessionMcpAttachment,
};
use awaken_session_execution_contract::{
    SessionRealizationAction, SessionRealizationControlFailure,
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
    session_id: &awaken_session_types::SessionId,
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
    session_id: &awaken_session_types::SessionId,
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
        &awaken_session_types::SessionId::from(session_id),
        attachments,
    )?;
    let publish = receipts
        .into_iter()
        .filter(|receipt| {
            expected_publish.iter().any(|current| {
                awaken_session_execution_contract::realization_generation_authorizes(
                    current, receipt,
                )
            })
        })
        .collect::<Vec<_>>();
    if !generation_set_authorizes(&expected_publish, &publish) {
        return Err(SessionRealizationControlFailure::Invalid(
            "activated publication set has no exact staged receipt set".into(),
        ));
    }
    let drain = draining_generations(
        &awaken_session_types::SessionId::from(session_id),
        attachments,
    )?;
    Ok(SessionRealizationAction::Publish { publish, drain })
}

#[cfg(test)]
mod collection_tests;

#[cfg(test)]
mod exhaustive {
    use super::*;

    const EVERY_STATE: [McpAttachmentState; 6] = [
        McpAttachmentState::Requested,
        McpAttachmentState::Realizing,
        McpAttachmentState::Active,
        McpAttachmentState::Draining,
        McpAttachmentState::Removed,
        McpAttachmentState::Failed,
    ];

    const fn listed_state(state: McpAttachmentState) -> usize {
        match state {
            McpAttachmentState::Requested => 0,
            McpAttachmentState::Realizing => 1,
            McpAttachmentState::Active => 2,
            McpAttachmentState::Draining => 3,
            McpAttachmentState::Removed => 4,
            McpAttachmentState::Failed => 5,
        }
    }

    #[test]
    fn every_attachment_state_is_listed() {
        for (index, state) in EVERY_STATE.into_iter().enumerate() {
            assert_eq!(listed_state(state), index);
        }
    }

    /// @exhaustive McpAttachmentState(6) x bool(2) = 12
    /// @obligation session_realization.publication_excludes_staged_retired_generation
    #[test]
    fn realization_publication_selects_only_unacknowledged_active_generation() {
        // R1 selects iff the state is Active and the publication is not yet
        // acknowledged; every staged, retired, terminal, or already
        // acknowledged row is excluded.
        let mut cases = 0;
        for state in EVERY_STATE {
            for acknowledged in [false, true] {
                assert_eq!(
                    realization_publication_selects(state, acknowledged),
                    state == McpAttachmentState::Active && !acknowledged
                );
                cases += 1;
            }
        }
        assert_eq!(cases, 12);
    }
}
